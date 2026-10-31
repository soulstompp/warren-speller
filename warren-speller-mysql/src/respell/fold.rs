// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A grouped SELECT over inner joins that splits into two sides joined only by equalities, grouped on
//! each side before the join.
//!
//! The SELECT's relations are cut into two sides, each joined within itself, and joined to each
//! other only by equalities of a column of one side with a column of the other. It is respelled
//! where all of these hold, read from the statement and the catalog:
//! - every GROUP BY item reads one side;
//! - every aggregate reads one side, and is `COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT u)`, `SUM(x)` of
//!   whole numbers and decimals, `MIN(x)` or `MAX(x)`;
//! - each side whose rows are counted (every side for `COUNT(*)` and `COUNT(x)`, the side a
//!   `COUNT(DISTINCT u)` reads, the other side for a `SUM`) has a relation from each of whose rows
//!   its joins reach at most one row of every other relation of the side, through equalities on a
//!   unique key of columns that hold no NULL; and `u` holds such a key of that relation;
//! - every join is an inner join, and nothing in the SELECT is a `?`, a subquery, a window, a user
//!   variable or a function whose value may change between calls.
//!
//! Each side is grouped by the columns it is joined on and the GROUP BY items it reads, holding its
//! row count and each aggregate it reads. The side holding the rows a count counts (the side a
//! `COUNT(DISTINCT u)` reads, else the one side whose rows are counted, else the first) is a WITH
//! query, and the other side is a derived table grouped only over the values of the joined columns
//! that WITH query holds. The two are joined on those columns and grouped again by the statement's
//! GROUP BY items: a count becomes the sum of the products of the sides' counts, `COUNT(DISTINCT u)`
//! the sum of its side's counts, a `SUM` the sum of its side's sum times the other side's count, and
//! `MIN` and `MAX` are taken again. The ORDER BY and LIMIT stay on the outside.
//!
//! A WITH query read once and a derived table, each a plain SELECT over inner joins, are read as
//! their relations where no cut is found otherwise; a WITH query read that way is removed.

use super::relations::{
    aggregate, column_at, conditions, connected, exact, fresh, has_aggregate, inner_joins_only,
    is_aggregate, plain, qualified, reads_table, relations_of, scan, tables_read, times_read,
    to_one, Aggregate, Ctx, Rel, What,
};
use super::{and_text, from_text};
use crate::sql;
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, Ident, OrderByExpr, OrderByKind, OrderBySort, Query, Select,
    SelectItem, SetExpr, Value, VisitMut, VisitorMut,
};
use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;

/// The most relations a SELECT may read for its cuts to be tried.
const MOST_RELATIONS: usize = 16;

/// The most times WITH queries and derived tables are read as their relations.
const MOST_MERGES: usize = 4;

/// An ORDER BY item.
#[derive(Clone)]
enum Order {
    /// A position or a name the select list gives, kept as written.
    Kept(Box<OrderByExpr>),
    /// An expression, qualified, and its direction.
    Expr(Box<Expr>, Option<OrderBySort>),
}

/// A SELECT read as its relations, its conditions, and its select list, GROUP BY and ORDER BY, each
/// column reference qualified.
#[derive(Clone)]
struct Block {
    rels: Vec<Rel>,
    conds: Vec<Expr>,
    items: Vec<(Expr, Ident)>,
    groups: Vec<Expr>,
    order: Vec<Order>,
    /// The WITH queries read as their relations.
    merged: Vec<String>,
}

/// The query grouped on each side of a cut before the join; none where it does not apply.
pub fn fold(q: &Query, ctx: &Ctx, used: &mut BTreeSet<String>) -> Option<Query> {
    if q.fetch.is_some()
        || !q.locks.is_empty()
        || q.for_clause.is_some()
        || q.settings.is_some()
        || q.format_clause.is_some()
        || !q.pipe_operators.is_empty()
    {
        return None;
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    let GroupByExpr::Expressions(group_items, _) = &s.group_by else {
        return None;
    };
    if !plain(s, true) || group_items.is_empty() || !inner_joins_only(&s.from) {
        return None;
    }
    for sc in [
        scan(s.as_ref()),
        q.order_by.as_ref().map(scan).unwrap_or_default(),
    ] {
        if !sc.movable() || sc.subquery {
            return None;
        }
    }
    let inner = ctx.inside(q);
    let mut block = read(s, q, &inner)?;
    for _ in 0..=MOST_MERGES {
        if let Some((text, held)) = cut(&block, used) {
            let parsed = sql::parse_query(&text)?;
            let mut added = sql::parse_query(&format!("WITH {held} SELECT 1"))?.with?;
            let mut out = q.clone();
            out.body = parsed.body;
            out.order_by = parsed.order_by;
            match &mut out.with {
                Some(with) => {
                    with.cte_tables
                        .retain(|c| !block.merged.contains(&c.alias.name.value));
                    with.cte_tables.append(&mut added.cte_tables);
                }
                None => out.with = Some(added),
            }
            return Some(out);
        }
        block = merged(&block, q, ctx)?;
    }
    None
}

/// The SELECT read as a block.
fn read(s: &Select, q: &Query, ctx: &Ctx) -> Option<Block> {
    let rels = ctx.relations(&s.from)?;
    let conds = conditions(s)
        .iter()
        .map(|c| qualified(c, &rels))
        .collect::<Option<Vec<_>>>()?;
    let mut items = Vec::new();
    for item in &s.projection {
        let (e, name) = match item {
            SelectItem::ExprWithAlias { expr, alias } => (expr, alias.clone()),
            SelectItem::UnnamedExpr(e @ Expr::Identifier(i)) => (e, i.clone()),
            SelectItem::UnnamedExpr(e @ Expr::CompoundIdentifier(p)) => (e, p.last()?.clone()),
            _ => return None,
        };
        items.push((qualified(e, &rels)?, name));
    }
    let GroupByExpr::Expressions(group_items, _) = &s.group_by else {
        return None;
    };
    let mut groups = Vec::new();
    for g in group_items {
        let e = match g {
            Expr::Value(v) => match &v.value {
                Value::Number(n, _) => {
                    let k: usize = n.parse().ok()?;
                    items.get(k.checked_sub(1)?)?.0.clone()
                }
                _ => return None,
            },
            // MySQL reads a GROUP BY name as a column of the FROM first, then as a name the select
            // list gives
            Expr::Identifier(i) if column_at(g, &rels).is_none() => {
                let named: Vec<&(Expr, Ident)> = items
                    .iter()
                    .filter(|(_, n)| n.value.eq_ignore_ascii_case(&i.value))
                    .collect();
                match named.as_slice() {
                    [(e, _)] => e.clone(),
                    _ => return None,
                }
            }
            e => qualified(e, &rels)?,
        };
        if has_aggregate(&e) {
            return None;
        }
        groups.push(e);
    }
    let mut order = Vec::new();
    if let Some(ob) = &q.order_by {
        if ob.interpolate.is_some() {
            return None;
        }
        let OrderByKind::Expressions(list) = &ob.kind else {
            return None;
        };
        for o in list {
            if o.options.nulls_first.is_some() || o.with_fill.is_some() {
                return None;
            }
            if matches!(o.options.sort, Some(OrderBySort::Using(_))) {
                return None;
            }
            let kept = match &o.expr {
                Expr::Value(v) => matches!(v.value, Value::Number(..)),
                Expr::Identifier(i) => items
                    .iter()
                    .any(|(_, n)| n.value.eq_ignore_ascii_case(&i.value)),
                _ => false,
            };
            order.push(if kept {
                Order::Kept(Box::new(o.clone()))
            } else {
                Order::Expr(Box::new(qualified(&o.expr, &rels)?), o.options.sort.clone())
            });
        }
    }
    Some(Block {
        rels,
        conds,
        items,
        groups,
        order,
        merged: Vec::new(),
    })
}

/// The block with each WITH query read once and each derived table read as their relations; none
/// when there is none to read so.
fn merged(block: &Block, q: &Query, ctx: &Ctx) -> Option<Block> {
    let mut out = block.clone();
    let mut changed = false;
    let mut r = 0;
    while r < out.rels.len() {
        let rel = out.rels[r].clone();
        let Some((body, body_ctx)) = mergeable(&rel, q, ctx) else {
            r += 1;
            continue;
        };
        let Some(inner) = body_ctx.relations(&body.from) else {
            r += 1;
            continue;
        };
        // a name it reads, at any depth, that is a WITH query in the statement's scope and not in its own
        let main = ctx.inside(q);
        if tables_read(body)
            .iter()
            .any(|t| main.has_cte(t) && !body_ctx.has_cte(t))
        {
            r += 1;
            continue;
        }
        let taken: BTreeSet<String> = out
            .rels
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != r)
            .map(|(_, x)| x.qualifier.value.to_lowercase())
            .collect();
        if inner
            .iter()
            .any(|x| taken.contains(&x.qualifier.value.to_lowercase()))
        {
            r += 1;
            continue;
        }
        let Some(values) = body
            .projection
            .iter()
            .map(|i| match i {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    qualified(e, &inner)
                }
                _ => None,
            })
            .collect::<Option<Vec<Expr>>>()
        else {
            r += 1;
            continue;
        };
        let Some(conds) = conditions(body)
            .iter()
            .map(|c| qualified(c, &inner))
            .collect::<Option<Vec<_>>>()
        else {
            r += 1;
            continue;
        };
        let by: HashMap<String, Expr> = rel
            .shape
            .columns
            .iter()
            .zip(&values)
            .filter_map(|(c, v)| Some((c.name.clone()?.to_lowercase(), v.clone())))
            .collect();
        let swap = |e: &Expr| substituted(e, &rel.qualifier.value, &by);
        let Some(outer_conds) = out.conds.iter().map(swap).collect::<Option<Vec<_>>>() else {
            r += 1;
            continue;
        };
        let Some(items) = out
            .items
            .iter()
            .map(|(e, n)| Some((swap(e)?, n.clone())))
            .collect::<Option<Vec<_>>>()
        else {
            r += 1;
            continue;
        };
        let Some(groups) = out.groups.iter().map(swap).collect::<Option<Vec<_>>>() else {
            r += 1;
            continue;
        };
        let Some(order) = out
            .order
            .iter()
            .map(|o| match o {
                Order::Kept(k) => Some(Order::Kept(k.clone())),
                Order::Expr(e, d) => Some(Order::Expr(Box::new(swap(e)?), d.clone())),
            })
            .collect::<Option<Vec<_>>>()
        else {
            r += 1;
            continue;
        };
        if let What::Cte(name) = &rel.what {
            out.merged.push(name.clone());
        }
        let n = inner.len();
        out.rels.splice(r..=r, inner);
        out.conds = outer_conds;
        out.conds.extend(conds);
        out.items = items;
        out.groups = groups;
        out.order = order;
        changed = true;
        r += n;
    }
    changed.then_some(out)
}

/// The SELECT a relation reads, and the scope it is read in, when it can be read as its relations:
/// a WITH query of this query that is not recursive and is read once, or a derived table; either a
/// plain SELECT over inner joins with nothing a respelling must leave where it stands.
fn mergeable<'a, 'c>(rel: &'a Rel, q: &'a Query, ctx: &Ctx<'c>) -> Option<(&'a Select, Ctx<'c>)> {
    let (query, body_ctx): (&Query, Ctx) = match &rel.what {
        What::Cte(name) => {
            let with = q.with.as_ref()?;
            let at = with
                .cte_tables
                .iter()
                .position(|c| c.alias.name.value == *name)?;
            let cte = &with.cte_tables[at];
            if (with.recursive && reads_table(&cte.query, name))
                || times_read(q, name) != 1
                || !cte.alias.columns.is_empty()
            {
                return None;
            }
            let mut before = q.clone();
            before.with.as_mut()?.cte_tables.truncate(at);
            (&cte.query, ctx.inside(&before))
        }
        What::Derived => {
            let sqlparser::ast::TableFactor::Derived {
                subquery,
                alias: Some(a),
                ..
            } = &rel.factor
            else {
                return None;
            };
            if !a.columns.is_empty() {
                return None;
            }
            (subquery.as_ref(), ctx.inside(q))
        }
        What::Table(_) => return None,
    };
    if query.with.is_some() || !super::relations::bare(query) {
        return None;
    }
    let SetExpr::Select(body) = query.body.as_ref() else {
        return None;
    };
    let sc = scan(body.as_ref());
    if !plain(body, false) || !inner_joins_only(&body.from) || !sc.movable() || sc.subquery {
        return None;
    }
    if body.projection.iter().any(|i| {
        !matches!(i, SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } if !has_aggregate(e))
    }) {
        return None;
    }
    Some((body, body_ctx))
}

/// An expression with each reference `qualifier.column` replaced by the column's value; none when a
/// reference names a column that has none.
fn substituted(e: &Expr, qualifier: &str, by: &HashMap<String, Expr>) -> Option<Expr> {
    struct Swap<'a> {
        qualifier: &'a str,
        by: &'a HashMap<String, Expr>,
        failed: bool,
    }
    impl VisitorMut for Swap<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
            if let Expr::CompoundIdentifier(p) = e {
                if p.len() == 2 && p[0].value == self.qualifier {
                    match self.by.get(&p[1].value.to_lowercase()) {
                        Some(v) => {
                            *e = match v {
                                Expr::Identifier(_)
                                | Expr::CompoundIdentifier(_)
                                | Expr::Value(_)
                                | Expr::Function(_)
                                | Expr::Nested(_) => v.clone(),
                                other => Expr::Nested(Box::new(other.clone())),
                            }
                        }
                        None => self.failed = true,
                    }
                    return ControlFlow::Continue(());
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let mut s = Swap {
        qualifier,
        by,
        failed: false,
    };
    let _ = e.visit(&mut s);
    (!s.failed).then_some(e)
}

// ---------------------------------------------------------------------------------------------
// The cut

/// A column of a relation of the block.
type Node = (usize, usize);

/// Every aggregate of the select list and the ORDER BY, each once, by its text.
fn aggregates(block: &Block) -> Option<Vec<(String, Aggregate)>> {
    struct Find {
        found: Vec<(String, Option<Aggregate>)>,
    }
    impl sqlparser::ast::Visitor for Find {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            if let Expr::Function(f) = e {
                if is_aggregate(f) {
                    let text = e.to_string();
                    if !self.found.iter().any(|(t, _)| *t == text) {
                        self.found.push((text, aggregate(f)));
                    }
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut f = Find { found: Vec::new() };
    for (e, _) in &block.items {
        let _ = sqlparser::ast::Visit::visit(e, &mut f);
    }
    for o in &block.order {
        if let Order::Expr(e, _) = o {
            let _ = sqlparser::ast::Visit::visit(e, &mut f);
        }
    }
    f.found.into_iter().map(|(t, a)| Some((t, a?))).collect()
}

/// What an aggregate reads.
fn reads(a: &Aggregate) -> Vec<&Expr> {
    match a {
        Aggregate::CountRows => Vec::new(),
        Aggregate::Count(e) | Aggregate::Sum(e) | Aggregate::Min(e) | Aggregate::Max(e) => vec![e],
        Aggregate::CountDistinct(es) => es.iter().collect(),
    }
}

/// A cut that holds: which relations are on the first side, the columns the sides are joined on,
/// and the equalities each side takes of those the join implied.
struct Cut {
    first: Vec<bool>,
    /// For each column the sides are joined on: the first side's, then the second's.
    keys: Vec<(Node, Node)>,
    /// The conditions within each side, those implied included.
    within: [Vec<Expr>; 2],
    /// Whether each side's rows are counted.
    counted: [bool; 2],
}

/// Which side of a cut a set of relations lies on: 0, 1, or none when it lies on both. A set of no
/// relation lies on the first side.
fn side_of(on: &BTreeSet<usize>, first: &[bool]) -> Option<usize> {
    if on.iter().all(|&r| first[r]) {
        Some(0)
    } else if on.iter().all(|&r| !first[r]) {
        Some(1)
    } else {
        None
    }
}

fn find(parent: &mut HashMap<Node, Node>, x: Node) -> Node {
    let p = *parent.get(&x).unwrap_or(&x);
    if p == x {
        return x;
    }
    let root = find(parent, p);
    parent.insert(x, root);
    root
}

fn union(parent: &mut HashMap<Node, Node>, a: Node, b: Node) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent.insert(ra, rb);
    }
}

/// The two columns an equality of plain column references names.
fn equated(c: &Expr, rels: &[Rel]) -> Option<(Node, Node)> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = c
    else {
        return None;
    };
    Some((column_at(left, rels)?, column_at(right, rels)?))
}

/// A reference to a column, as a condition of the block writes it.
fn node_expr(rels: &[Rel], node: Node, conds: &[Expr]) -> Expr {
    for c in conds {
        if let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = c
        {
            for side in [left, right] {
                if column_at(side, rels) == Some(node) {
                    return side.as_ref().clone();
                }
            }
        }
    }
    let name = rels[node.0].shape.columns[node.1]
        .name
        .clone()
        .unwrap_or_default();
    Expr::CompoundIdentifier(vec![rels[node.0].qualifier.clone(), sql::ident(&name)])
}

/// The cut `first` names, when every condition holds.
fn check(block: &Block, first: Vec<bool>, aggs: &[(String, Aggregate)]) -> Option<Cut> {
    let rels = &block.rels;
    let mut within: [Vec<Expr>; 2] = [Vec::new(), Vec::new()];
    let mut crossing: Vec<(Node, Node)> = Vec::new();
    for c in &block.conds {
        match side_of(&relations_of(c, rels), &first) {
            Some(s) => within[s].push(c.clone()),
            None => {
                let (a, b) = equated(c, rels)?;
                let (a, b) = if first[a.0] { (a, b) } else { (b, a) };
                if !first[a.0] || first[b.0] {
                    return None;
                }
                crossing.push((a, b));
            }
        }
    }
    if crossing.is_empty() {
        return None;
    }
    for g in &block.groups {
        side_of(&relations_of(g, rels), &first)?;
    }
    let mut counted = [false, false];
    let mut distinct: Vec<(usize, Vec<Node>)> = Vec::new();
    for (_, a) in aggs {
        let on: BTreeSet<usize> = reads(a)
            .iter()
            .flat_map(|e| relations_of(e, rels))
            .collect();
        let s = side_of(&on, &first)?;
        match a {
            Aggregate::CountRows => counted = [true, true],
            Aggregate::Count(_) => counted = [true, true],
            Aggregate::CountDistinct(es) => {
                counted[s] = true;
                distinct.push((
                    s,
                    es.iter()
                        .map(|e| column_at(e, rels))
                        .collect::<Option<_>>()?,
                ));
            }
            Aggregate::Sum(_) => counted[1 - s] = true,
            Aggregate::Min(_) | Aggregate::Max(_) => {}
        }
    }

    // the columns joined across the cut, a class of columns equal to each other at a time
    let mut all: HashMap<Node, Node> = HashMap::new();
    let mut local: [HashMap<Node, Node>; 2] = [HashMap::new(), HashMap::new()];
    for (s, conds) in within.iter().enumerate() {
        for c in conds {
            if let Some((a, b)) = equated(c, rels) {
                union(&mut all, a, b);
                union(&mut local[s], a, b);
            }
        }
    }
    for &(a, b) in &crossing {
        union(&mut all, a, b);
    }
    let mut keys: Vec<(Node, Node)> = Vec::new();
    let mut seen: Vec<Node> = Vec::new();
    for &(a, b) in &crossing {
        let class = find(&mut all, a);
        if seen.contains(&class) {
            continue;
        }
        seen.push(class);
        let members: Vec<Node> = crossing
            .iter()
            .flat_map(|&(x, y)| [x, y])
            .chain(
                within
                    .iter()
                    .flatten()
                    .filter_map(|c| equated(c, rels))
                    .flat_map(|(x, y)| [x, y]),
            )
            .filter(|&n| find(&mut all, n) == class)
            .collect::<Vec<_>>();
        let mut members_dedup: Vec<Node> = Vec::new();
        for m in members {
            if !members_dedup.contains(&m) {
                members_dedup.push(m);
            }
        }
        // every column of the class compares as one known type, so that each side groups them
        // as the join compares them
        let kind = &rels[a.0].shape.columns[a.1].kind;
        if kind.is_none()
            || members_dedup
                .iter()
                .any(|&x| rels[x.0].shape.columns[x.1].kind != *kind)
        {
            return None;
        }
        for (s, rep) in [(0, a), (1, b)] {
            let others: Vec<Node> = members_dedup
                .iter()
                .copied()
                .filter(|&m| (first[m.0] == (s == 0)) && m != rep)
                .collect();
            for m in others {
                if find(&mut local[s], m) != find(&mut local[s], rep) {
                    within[s].push(Expr::BinaryOp {
                        left: Box::new(node_expr(rels, m, &block.conds)),
                        op: BinaryOperator::Eq,
                        right: Box::new(node_expr(rels, rep, &block.conds)),
                    });
                    union(&mut local[s], m, rep);
                }
            }
        }
        keys.push((a, b));
    }

    let members: [Vec<usize>; 2] = [
        (0..rels.len()).filter(|&r| first[r]).collect(),
        (0..rels.len()).filter(|&r| !first[r]).collect(),
    ];
    for s in 0..2 {
        if !connected(&members[s], &within[s], rels) {
            return None;
        }
        let grains: Vec<usize> = members[s]
            .iter()
            .copied()
            .filter(|&g| to_one(rels, &within[s], g, &members[s]))
            .collect();
        if counted[s] && grains.is_empty() {
            return None;
        }
        for (ds, u) in &distinct {
            if *ds != s {
                continue;
            }
            let keyed = grains.iter().any(|&g| {
                u.iter()
                    .all(|n| n.0 == g && rels[g].shape.columns[n.1].not_null)
                    && rels[g]
                        .shape
                        .unique
                        .iter()
                        .any(|k| k.iter().all(|c| u.iter().any(|n| n.1 == *c)))
            });
            if !keyed {
                return None;
            }
        }
    }
    Some(Cut {
        first,
        keys,
        within,
        counted,
    })
}

/// The block's cuts that hold, the one with the fewest relations on its smaller side, the first of
/// those in the order of the relations; and the respelled SELECT's text, with the WITH query it adds.
fn cut(block: &Block, used: &mut BTreeSet<String>) -> Option<(String, String)> {
    let n = block.rels.len();
    if !(2..=MOST_RELATIONS).contains(&n) {
        return None;
    }
    let aggs = aggregates(block)?;
    for (_, a) in &aggs {
        if let Aggregate::Sum(e) = a {
            if !exact(e, &block.rels) {
                return None;
            }
        }
        if let Aggregate::CountDistinct(es) = a {
            if es.iter().any(|e| column_at(e, &block.rels).is_none()) {
                return None;
            }
        }
    }
    let mut best: Option<(usize, Vec<usize>, Cut)> = None;
    for mask in 0u32..(1 << (n - 1)) {
        let first: Vec<bool> = (0..n)
            .map(|r| r == 0 || mask & (1 << (r - 1)) != 0)
            .collect();
        if first.iter().all(|&f| f) {
            continue;
        }
        let Some(c) = check(block, first, &aggs) else {
            continue;
        };
        let a: Vec<usize> = (0..n).filter(|&r| c.first[r]).collect();
        let b: Vec<usize> = (0..n).filter(|&r| !c.first[r]).collect();
        let smaller = if a.len() <= b.len() { a } else { b };
        let better = match &best {
            None => true,
            Some((size, list, _)) => (smaller.len(), &smaller) < (*size, list),
        };
        if better {
            best = Some((smaller.len(), smaller, c));
        }
    }
    let (_, _, c) = best?;
    respelled(block, &c, &aggs, used)
}

// ---------------------------------------------------------------------------------------------
// The respelled SELECT

fn respelled(
    block: &Block,
    c: &Cut,
    aggs: &[(String, Aggregate)],
    used: &mut BTreeSet<String>,
) -> Option<(String, String)> {
    let rels = &block.rels;
    let names = [fresh("side_a", used), fresh("side_b", used)];
    let alias = [sql::ident(&names[0]), sql::ident(&names[1])];
    let side = |on: &BTreeSet<usize>| side_of(on, &c.first).unwrap_or(0);

    // each side's columns: the columns it is joined on, its GROUP BY items, its count, its
    // aggregates
    let mut columns: [Vec<(String, String)>; 2] = [Vec::new(), Vec::new()];
    for (i, (a, b)) in c.keys.iter().enumerate() {
        columns[0].push((
            node_expr(rels, *a, &block.conds).to_string(),
            format!("k{}", i + 1),
        ));
        columns[1].push((
            node_expr(rels, *b, &block.conds).to_string(),
            format!("k{}", i + 1),
        ));
    }
    let mut group_at: Vec<String> = Vec::new();
    for (i, g) in block.groups.iter().enumerate() {
        let s = side(&relations_of(g, rels));
        columns[s].push((g.to_string(), format!("g{}", i + 1)));
        group_at.push(format!("{}.g{}", alias[s], i + 1));
    }
    let grouped: [usize; 2] = [columns[0].len(), columns[1].len()];
    for (cols, counted) in columns.iter_mut().zip(c.counted) {
        if counted {
            cols.push(("COUNT(*)".into(), "n".into()));
        }
    }
    let count = |s: usize| format!("{}.n", alias[s]);
    let mut replaced: HashMap<String, String> = HashMap::new();
    for (i, (text, a)) in aggs.iter().enumerate() {
        let on: BTreeSet<usize> = reads(a)
            .iter()
            .flat_map(|e| relations_of(e, rels))
            .collect();
        let s = side(&on);
        let partial = format!("p{}", i + 1);
        let at = format!("{}.{partial}", alias[s]);
        let whole = match a {
            Aggregate::CountRows => format!("CAST(SUM({} * {}) AS SIGNED)", count(0), count(1)),
            Aggregate::Count(e) => {
                columns[s].push((format!("COUNT({e})"), partial));
                format!("CAST(SUM({at} * {}) AS SIGNED)", count(1 - s))
            }
            Aggregate::CountDistinct(_) => format!("CAST(SUM({}) AS SIGNED)", count(s)),
            Aggregate::Sum(e) => {
                columns[s].push((format!("SUM({e})"), partial));
                format!("SUM({at} * {})", count(1 - s))
            }
            Aggregate::Min(e) => {
                columns[s].push((format!("MIN({e})"), partial));
                format!("MIN({at})")
            }
            Aggregate::Max(e) => {
                columns[s].push((format!("MAX({e})"), partial));
                format!("MAX({at})")
            }
        };
        replaced.insert(text.clone(), whole);
    }

    // the side holding the grain a count counts: the side a COUNT(DISTINCT u) reads, else the one
    // side whose rows are counted, else the first. It becomes a WITH query, and the other side is
    // grouped only over the values of the joined columns it holds.
    let held = aggs
        .iter()
        .find_map(|(_, a)| match a {
            Aggregate::CountDistinct(es) => Some(side(
                &es.iter().flat_map(|e| relations_of(e, rels)).collect(),
            )),
            _ => None,
        })
        .unwrap_or(match c.counted {
            [false, true] => 1,
            _ => 0,
        });
    let keys: Vec<String> = (1..=c.keys.len()).map(|i| format!("k{i}")).collect();
    let mut sides = Vec::new();
    let mut with = String::new();
    for s in 0..2 {
        let members: Vec<usize> = (0..rels.len())
            .filter(|&r| c.first[r] == (s == 0))
            .collect();
        let (from, filter) = from_text(rels, &members, &c.within[s])?;
        let items: Vec<&str> = columns[s].iter().map(|(e, _)| e.as_str()).collect();
        let by: Vec<&str> = columns[s][..grouped[s]]
            .iter()
            .map(|(e, _)| e.as_str())
            .collect();
        let mut conditions: Vec<String> = if filter.is_empty() {
            Vec::new()
        } else {
            vec![and_text(&filter)]
        };
        if s != held {
            let joined: Vec<&str> = columns[s][..keys.len()]
                .iter()
                .map(|(e, _)| e.as_str())
                .collect();
            let joined = match joined.as_slice() {
                [one] => one.to_string(),
                many => format!("({})", many.join(", ")),
            };
            conditions.push(format!(
                "{joined} IN (SELECT {} FROM {})",
                keys.join(", "),
                alias[held]
            ));
        }
        let mut text = format!("SELECT {} FROM {from}", items.join(", "));
        if !conditions.is_empty() {
            text.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
        }
        text.push_str(&format!(" GROUP BY {}", by.join(", ")));
        let list: Vec<&str> = columns[s].iter().map(|(_, n)| n.as_str()).collect();
        if s == held {
            with = format!("{} ({}) AS ({text})", alias[s], list.join(", "));
            sides.push(alias[s].to_string());
        } else {
            sides.push(format!("({text}) AS {} ({})", alias[s], list.join(", ")));
        }
    }

    let groups: Vec<(String, String)> = block
        .groups
        .iter()
        .zip(&group_at)
        .map(|(g, at)| (g.to_string(), at.clone()))
        .collect();
    let mut items = Vec::new();
    for (e, name) in &block.items {
        items.push(format!("{} AS {name}", outer(e, &groups, &replaced)?));
    }
    let on: Vec<String> = (1..=c.keys.len())
        .map(|i| format!("{}.k{i} = {}.k{i}", alias[0], alias[1]))
        .collect();
    let mut text = format!(
        "SELECT {} FROM {} JOIN {} ON {} GROUP BY {}",
        items.join(", "),
        sides[0],
        sides[1],
        on.join(" AND "),
        group_at.join(", ")
    );
    if !block.order.is_empty() {
        let mut order = Vec::new();
        for o in &block.order {
            order.push(match o {
                Order::Kept(k) => k.to_string(),
                Order::Expr(e, sort) => {
                    let e = outer(e, &groups, &replaced)?;
                    match sort {
                        Some(OrderBySort::Asc) => format!("{e} ASC"),
                        Some(OrderBySort::Desc) => format!("{e} DESC"),
                        _ => e.to_string(),
                    }
                }
            });
        }
        text.push_str(&format!(" ORDER BY {}", order.join(", ")));
    }
    Some((text, with))
}

/// An expression of the select list or the ORDER BY written over the two sides: each GROUP BY item
/// as its side's column, each aggregate as its respelling; none when a column is read otherwise.
fn outer(e: &Expr, groups: &[(String, String)], aggs: &HashMap<String, String>) -> Option<Expr> {
    struct Rewrite<'a> {
        groups: &'a [(String, String)],
        aggs: &'a HashMap<String, String>,
        marks: Vec<Expr>,
        failed: bool,
    }
    const MARK: &str = "\u{1}";
    impl VisitorMut for Rewrite<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
            let text = e.to_string();
            let with = if let Some((_, at)) = self.groups.iter().find(|(g, _)| *g == text) {
                Some(at.clone())
            } else if let Expr::Function(f) = e {
                if is_aggregate(f) {
                    match self.aggs.get(&text) {
                        Some(a) => Some(a.clone()),
                        None => {
                            self.failed = true;
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(with) = with {
                match sql::parse_expr(&with) {
                    Some(parsed) => {
                        self.marks.push(parsed);
                        *e = Expr::Identifier(sqlparser::ast::Ident::new(format!(
                            "{MARK}{}",
                            self.marks.len() - 1
                        )));
                    }
                    None => self.failed = true,
                }
            } else if matches!(e, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                self.failed = true;
            }
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
            if let Expr::Identifier(i) = e {
                if let Some(k) = i
                    .value
                    .strip_prefix(MARK)
                    .and_then(|k| k.parse::<usize>().ok())
                {
                    *e = self.marks[k].clone();
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let mut r = Rewrite {
        groups,
        aggs,
        marks: Vec::new(),
        failed: false,
    };
    let _ = e.visit(&mut r);
    (!r.failed).then_some(e)
}
