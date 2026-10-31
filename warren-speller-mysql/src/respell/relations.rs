// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a SELECT reads, read against the catalog: each table, WITH query or derived table in its
//! FROM with the columns it returns (their names, the type each compares as, whether each may hold
//! NULL) and its unique keys; the conditions of its WHERE and its joins' ONs, as one list; and every
//! column reference written as `relation.column`.

use crate::catalog::{Catalog, Kind};
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, DuplicateTreatment, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator, ObjectName, Query, Select,
    SelectFlavor, SelectItem, SetExpr, SetOperator, SetQuantifier, TableFactor, TableWithJoins,
    Value, Visit, VisitMut, Visitor, VisitorMut,
};
use std::collections::BTreeSet;
use std::ops::ControlFlow;

/// A column a relation returns.
#[derive(Debug, Clone)]
pub struct Out {
    /// Its name; none where MySQL would name it by the text of its expression.
    pub name: Option<String>,
    pub not_null: bool,
    /// How it compares; none where that cannot be told.
    pub kind: Option<Kind>,
}

/// The columns a relation returns, and its unique keys, each a set of positions among them whose
/// values hold no NULL.
#[derive(Debug, Clone, Default)]
pub struct Shape {
    pub columns: Vec<Out>,
    pub unique: Vec<BTreeSet<usize>>,
}

impl Shape {
    /// The position of a column by the name a statement gives it.
    pub fn position(&self, name: &str) -> Option<usize> {
        let found: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .map(|(i, _)| i)
            .collect();
        match found.as_slice() {
            [i] => Some(*i),
            _ => None,
        }
    }

    /// The columns renamed by a list of names, in order.
    fn renamed(mut self, names: &[Ident]) -> Option<Shape> {
        if names.is_empty() {
            return Some(self);
        }
        if names.len() != self.columns.len() {
            return None;
        }
        for (c, n) in self.columns.iter_mut().zip(names) {
            c.name = Some(n.value.clone());
        }
        Some(self)
    }
}

/// A WITH query in scope: its name, and what it returns when that can be read.
#[derive(Debug, Clone)]
pub struct Cte {
    pub name: String,
    pub shape: Option<Shape>,
}

/// What a respelling reads a statement against: the catalog, and the WITH queries in scope, the
/// nearest last.
#[derive(Clone)]
pub struct Ctx<'c> {
    pub catalog: &'c Catalog,
    pub ctes: Vec<Cte>,
}

impl<'c> Ctx<'c> {
    pub fn new(catalog: &'c Catalog) -> Ctx<'c> {
        Ctx {
            catalog,
            ctes: Vec::new(),
        }
    }

    fn cte(&self, name: &str) -> Option<&Cte> {
        self.ctes.iter().rev().find(|c| c.name == name)
    }

    /// Whether a WITH query of this name is in scope.
    pub fn has_cte(&self, name: &str) -> bool {
        self.cte(name).is_some()
    }

    /// The scope inside a query: this scope and the query's own WITH queries, each read in the
    /// scope of the ones before it.
    pub fn inside(&self, q: &Query) -> Ctx<'c> {
        let mut ctx = self.clone();
        if let Some(with) = &q.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.clone();
                let shape = if with.recursive && reads_table(&cte.query, &name) {
                    recursive_shape(&cte.query, &ctx, &cte.alias.columns_names())
                } else {
                    ctx.shape(&cte.query)
                        .and_then(|s| s.renamed(&cte.alias.columns_names()))
                };
                ctx.ctes.push(Cte { name, shape });
            }
        }
        ctx
    }

    /// What a query returns.
    pub fn shape(&self, q: &Query) -> Option<Shape> {
        let ctx = self.inside(q);
        ctx.body_shape(&q.body)
    }

    fn body_shape(&self, body: &SetExpr) -> Option<Shape> {
        match body {
            SetExpr::Select(s) => self.select_shape(s),
            SetExpr::Query(q) => self.shape(q),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let (l, r) = (self.body_shape(left)?, self.body_shape(right)?);
                if l.columns.len() != r.columns.len() {
                    return None;
                }
                let all = matches!(op, SetOperator::Union) && *set_quantifier == SetQuantifier::All;
                let columns = l
                    .columns
                    .iter()
                    .zip(&r.columns)
                    .map(|(a, b)| Out {
                        name: a.name.clone(),
                        not_null: all && a.not_null && b.not_null,
                        kind: if a.kind == b.kind {
                            a.kind.clone()
                        } else {
                            None
                        },
                    })
                    .collect();
                Some(Shape {
                    columns,
                    unique: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn select_shape(&self, s: &Select) -> Option<Shape> {
        let inner = inner_joins_only(&s.from);
        let rels = self.relations(&s.from)?;
        let mut columns = Vec::new();
        // for each column returned, the relation's column it is, when it is a plain reference
        let mut plain: Vec<Option<(usize, usize)>> = Vec::new();
        for item in &s.projection {
            match item {
                SelectItem::Wildcard(_) => {
                    for (r, rel) in rels.iter().enumerate() {
                        for (i, c) in rel.shape.columns.iter().enumerate() {
                            if rel.listed.get(i).copied().unwrap_or(true) {
                                columns.push(c.clone());
                                plain.push(Some((r, i)));
                            }
                        }
                    }
                }
                SelectItem::QualifiedWildcard(..) => return None,
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    let at = column_at(e, &rels);
                    let name = match (item, e) {
                        (SelectItem::ExprWithAlias { alias, .. }, _) => Some(alias.value.clone()),
                        (_, Expr::Identifier(i)) => Some(i.value.clone()),
                        (_, Expr::CompoundIdentifier(p)) => p.last().map(|i| i.value.clone()),
                        _ => None,
                    };
                    let (not_null, kind) = match at {
                        Some((r, i)) => {
                            let c = &rels[r].shape.columns[i];
                            (inner && c.not_null, c.kind.clone())
                        }
                        None => (literal_or_count(e), kind_of(e, &rels)),
                    };
                    columns.push(Out {
                        name,
                        not_null,
                        kind,
                    });
                    plain.push(at);
                }
                _ => return None,
            }
        }
        let mut unique: Vec<BTreeSet<usize>> = Vec::new();
        let keyed = |cols: &BTreeSet<usize>| cols.iter().all(|&i| columns[i].not_null);
        match &s.group_by {
            GroupByExpr::Expressions(items, modifiers) if !items.is_empty() => {
                if modifiers.is_empty() && s.distinct.is_none() {
                    let mut key = BTreeSet::new();
                    for g in items {
                        let at = match g {
                            Expr::Value(v) => match &v.value {
                                Value::Number(n, _) => n
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|k| k.checked_sub(1))
                                    .filter(|&k| k < plain.len()),
                                _ => None,
                            },
                            e => column_at(e, &rels)
                                .and_then(|c| plain.iter().position(|p| *p == Some(c))),
                        };
                        match at {
                            Some(k) => {
                                key.insert(k);
                            }
                            None => {
                                key.clear();
                                break;
                            }
                        }
                    }
                    if !key.is_empty() && keyed(&key) {
                        unique.push(key);
                    }
                }
            }
            GroupByExpr::Expressions(..) => {
                let aggregated = s.projection.iter().any(|i| {
                    matches!(i, SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } if has_aggregate(e))
                });
                if s.distinct.is_none() && inner && !aggregated {
                    let conds = conditions(s);
                    for g in 0..rels.len() {
                        if !to_one(&rels, &conds, g, &(0..rels.len()).collect::<Vec<_>>()) {
                            continue;
                        }
                        for key in &rels[g].shape.unique {
                            let out: Option<BTreeSet<usize>> = key
                                .iter()
                                .map(|&c| plain.iter().position(|p| *p == Some((g, c))))
                                .collect();
                            if let Some(out) = out {
                                if keyed(&out) && !unique.contains(&out) {
                                    unique.push(out);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if matches!(s.distinct, Some(sqlparser::ast::Distinct::Distinct)) {
            let all: BTreeSet<usize> = (0..columns.len()).collect();
            if keyed(&all) {
                unique.push(all);
            }
        }
        Some(Shape { columns, unique })
    }

    /// The relations of a FROM, in order; none when one of them cannot be read.
    pub fn relations(&self, from: &[TableWithJoins]) -> Option<Vec<Rel>> {
        let mut out = Vec::new();
        for twj in from {
            self.twj_relations(twj, &mut out)?;
        }
        Some(out)
    }

    fn twj_relations(&self, twj: &TableWithJoins, out: &mut Vec<Rel>) -> Option<()> {
        self.factor_relations(&twj.relation, out)?;
        for j in &twj.joins {
            self.factor_relations(&j.relation, out)?;
        }
        Some(())
    }

    fn factor_relations(&self, tf: &TableFactor, out: &mut Vec<Rel>) -> Option<()> {
        match tf {
            TableFactor::NestedJoin {
                table_with_joins,
                alias: None,
            } => self.twj_relations(table_with_joins, out),
            _ => {
                out.push(self.relation(tf)?);
                Some(())
            }
        }
    }

    /// A table, a WITH query or a derived table, read; none for anything else, or a relation whose
    /// columns cannot be read.
    pub fn relation(&self, tf: &TableFactor) -> Option<Rel> {
        match tf {
            TableFactor::Table {
                name,
                alias,
                args: None,
                with_hints,
                version: None,
                with_ordinality: false,
                json_path: None,
                sample: None,
                ..
            } if with_hints.is_empty() => {
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    return None;
                }
                let parts = idents(name)?;
                let (shape, listed, what) = match parts.as_slice() {
                    [t] if self.cte(&t.value).is_some() => {
                        let shape = self.cte(&t.value)?.shape.clone()?;
                        let listed = vec![true; shape.columns.len()];
                        (shape, listed, What::Cte(t.value.clone()))
                    }
                    [t] => self.base(&t.value)?,
                    [db, t] if db.value == self.catalog.database => self.base(&t.value)?,
                    _ => return None,
                };
                let qualifier = match alias {
                    Some(a) => a.name.clone(),
                    None => parts.last()?.clone(),
                };
                Some(Rel {
                    factor: tf.clone(),
                    qualifier,
                    aliased: alias.is_some(),
                    written: parts,
                    shape,
                    listed,
                    what,
                })
            }
            TableFactor::Derived {
                lateral: false,
                subquery,
                alias: Some(alias),
                sample: None,
            } => {
                let shape = self.shape(subquery)?.renamed(&alias.columns_names())?;
                let listed = vec![true; shape.columns.len()];
                Some(Rel {
                    factor: tf.clone(),
                    qualifier: alias.name.clone(),
                    aliased: true,
                    written: Vec::new(),
                    shape,
                    listed,
                    what: What::Derived,
                })
            }
            _ => None,
        }
    }

    fn base(&self, table: &str) -> Option<(Shape, Vec<bool>, What)> {
        let t = self.catalog.table(table)?;
        let columns = t
            .columns
            .iter()
            .map(|c| Out {
                name: Some(c.name.clone()),
                not_null: c.not_null,
                kind: Some(c.kind.clone()),
            })
            .collect();
        let unique = t
            .unique
            .iter()
            .filter_map(|k| {
                k.iter()
                    .map(|c| t.columns.iter().position(|x| x.name == *c))
                    .collect::<Option<BTreeSet<usize>>>()
            })
            .collect();
        let listed = t.columns.iter().map(|c| c.listed).collect();
        Some((
            Shape { columns, unique },
            listed,
            What::Table(table.to_string()),
        ))
    }
}

trait ColumnNames {
    fn columns_names(&self) -> Vec<Ident>;
}

impl ColumnNames for sqlparser::ast::TableAlias {
    fn columns_names(&self) -> Vec<Ident> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }
}

/// What a recursive WITH query returns, read from the arm that does not read it: its names and
/// types, with no key and no column held free of NULL.
fn recursive_shape(q: &Query, ctx: &Ctx, names: &[Ident]) -> Option<Shape> {
    let mut body = q.body.as_ref();
    while let SetExpr::SetOperation { left, .. } = body {
        body = left.as_ref();
    }
    let mut shape = ctx.body_shape(body)?;
    for c in &mut shape.columns {
        c.not_null = false;
    }
    shape.unique.clear();
    shape.renamed(names)
}

/// Where a relation's rows come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    Table(String),
    Cte(String),
    Derived,
}

/// A relation a SELECT reads.
#[derive(Debug, Clone)]
pub struct Rel {
    /// The FROM item as written.
    pub factor: TableFactor,
    /// The name that qualifies its columns: its alias, or its table's name.
    pub qualifier: Ident,
    /// Whether it is read under an alias.
    pub aliased: bool,
    /// The table's name as written; empty for a derived table.
    pub written: Vec<Ident>,
    pub shape: Shape,
    /// Whether `SELECT *` returns each column.
    pub listed: Vec<bool>,
    pub what: What,
}

fn idents(name: &ObjectName) -> Option<Vec<Ident>> {
    name.0.iter().map(|p| p.as_ident().cloned()).collect()
}

/// Whether every join of a FROM is an inner join, on a condition or on none.
pub fn inner_joins_only(from: &[TableWithJoins]) -> bool {
    fn factor(tf: &TableFactor) -> bool {
        match tf {
            TableFactor::NestedJoin {
                table_with_joins,
                alias: None,
            } => joins(table_with_joins),
            TableFactor::NestedJoin { .. } => false,
            _ => true,
        }
    }
    fn joins(twj: &TableWithJoins) -> bool {
        factor(&twj.relation)
            && twj.joins.iter().all(|j| {
                factor(&j.relation)
                    && !j.global
                    && match &j.join_operator {
                        JoinOperator::Join(c)
                        | JoinOperator::Inner(c)
                        | JoinOperator::CrossJoin(c) => {
                            matches!(c, JoinConstraint::On(_) | JoinConstraint::None)
                        }
                        _ => false,
                    }
            })
    }
    from.iter().all(joins)
}

/// The conditions of a SELECT whose joins are inner joins: its WHERE and each join's ON, split at
/// each AND.
pub fn conditions(s: &Select) -> Vec<Expr> {
    fn ons(twj: &TableWithJoins, out: &mut Vec<Expr>) {
        if let TableFactor::NestedJoin {
            table_with_joins, ..
        } = &twj.relation
        {
            ons(table_with_joins, out);
        }
        for j in &twj.joins {
            if let TableFactor::NestedJoin {
                table_with_joins, ..
            } = &j.relation
            {
                ons(table_with_joins, out);
            }
            if let JoinOperator::Join(JoinConstraint::On(e))
            | JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::CrossJoin(JoinConstraint::On(e)) = &j.join_operator
            {
                out.extend(conjuncts(e));
            }
        }
    }
    let mut out = Vec::new();
    for twj in &s.from {
        ons(twj, &mut out);
    }
    if let Some(w) = &s.selection {
        out.extend(conjuncts(w));
    }
    out
}

/// The expressions ANDed in a condition.
pub fn conjuncts(e: &Expr) -> Vec<Expr> {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut out = conjuncts(left);
            out.extend(conjuncts(right));
            out
        }
        Expr::Nested(inner)
            if matches!(
                inner.as_ref(),
                Expr::BinaryOp {
                    op: BinaryOperator::And,
                    ..
                }
            ) =>
        {
            conjuncts(inner)
        }
        _ => vec![e.clone()],
    }
}

/// The relation and column a plain column reference names among `rels`.
pub fn column_at(e: &Expr, rels: &[Rel]) -> Option<(usize, usize)> {
    match e {
        Expr::Nested(inner) => column_at(inner, rels),
        Expr::Identifier(id) => {
            let found: Vec<(usize, usize)> = rels
                .iter()
                .enumerate()
                .filter_map(|(r, rel)| Some((r, rel.shape.position(&id.value)?)))
                .collect();
            match found.as_slice() {
                [one] => Some(*one),
                _ => None,
            }
        }
        Expr::CompoundIdentifier(parts) => {
            let (matches, column): (Vec<usize>, &Ident) = match parts.as_slice() {
                [q, c] => (
                    rels.iter()
                        .enumerate()
                        .filter(|(_, r)| r.qualifier.value == q.value)
                        .map(|(i, _)| i)
                        .collect(),
                    c,
                ),
                [db, t, c] => (
                    rels.iter()
                        .enumerate()
                        .filter(|(_, r)| {
                            !r.aliased
                                && matches!(r.written.as_slice(), [d, n] if d.value == db.value && n.value == t.value)
                        })
                        .map(|(i, _)| i)
                        .collect(),
                    c,
                ),
                _ => return None,
            };
            let [r] = matches.as_slice() else { return None };
            Some((*r, rels[*r].shape.position(&column.value)?))
        }
        _ => None,
    }
}

/// Whether an expression is a literal that is not NULL, or a count.
fn literal_or_count(e: &Expr) -> bool {
    match e {
        Expr::Nested(inner) => literal_or_count(inner),
        Expr::Value(v) => !matches!(v.value, Value::Null | Value::Placeholder(_)),
        Expr::Function(f) => function_name(f).as_deref() == Some("COUNT") && f.over.is_none(),
        _ => false,
    }
}

/// How an expression's value compares, where that can be told: a column's own, a whole number's,
/// or a count's.
pub fn kind_of(e: &Expr, rels: &[Rel]) -> Option<Kind> {
    if let Some((r, c)) = column_at(e, rels) {
        return rels[r].shape.columns[c].kind.clone();
    }
    match e {
        Expr::Nested(inner) => kind_of(inner, rels),
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) if n.bytes().all(|b| b.is_ascii_digit()) => Some(Kind::Integer),
            Value::Number(n, _) if n.bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
                Some(Kind::Decimal)
            }
            _ => None,
        },
        Expr::Function(f) if function_name(f).as_deref() == Some("COUNT") && f.over.is_none() => {
            Some(Kind::Integer)
        }
        Expr::Cast {
            kind: CastKind::Cast,
            data_type: DataType::Signed | DataType::SignedInteger,
            ..
        } => Some(Kind::Integer),
        _ => None,
    }
}

/// Whether an expression's arithmetic is exact: whole numbers and decimals, and their sums,
/// differences and products.
pub fn exact(e: &Expr, rels: &[Rel]) -> bool {
    match e {
        Expr::Nested(inner) => exact(inner, rels),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } => exact(expr, rels),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply,
            right,
        } => exact(left, rels) && exact(right, rels),
        _ => matches!(kind_of(e, rels), Some(Kind::Integer | Kind::Decimal)),
    }
}

/// A function's name, in capitals, when it is one plain name.
pub fn function_name(f: &sqlparser::ast::Function) -> Option<String> {
    let [part] = f.name.0.as_slice() else {
        return None;
    };
    Some(part.as_ident()?.value.to_ascii_uppercase())
}

/// MySQL's aggregate functions.
const AGGREGATES: [&str; 20] = [
    "AVG",
    "BIT_AND",
    "BIT_OR",
    "BIT_XOR",
    "COUNT",
    "GROUP_CONCAT",
    "JSON_ARRAYAGG",
    "JSON_OBJECTAGG",
    "MAX",
    "MIN",
    "STD",
    "STDDEV",
    "STDDEV_POP",
    "STDDEV_SAMP",
    "SUM",
    "VAR_POP",
    "VAR_SAMP",
    "VARIANCE",
    "ANY_VALUE",
    "GROUPING",
];

/// Whether a function call is an aggregate: one of MySQL's aggregate functions, with no window.
pub fn is_aggregate(f: &sqlparser::ast::Function) -> bool {
    f.over.is_none() && function_name(f).is_some_and(|n| AGGREGATES.contains(&n.as_str()))
}

/// Whether an expression holds an aggregate outside any query nested in it.
pub fn has_aggregate(e: &Expr) -> bool {
    struct Find {
        depth: usize,
        found: bool,
    }
    impl Visitor for Find {
        type Break = ();
        fn pre_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            if let Expr::Function(f) = e {
                if self.depth == 0 && is_aggregate(f) {
                    self.found = true;
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut f = Find {
        depth: 0,
        found: false,
    };
    let _ = e.visit(&mut f);
    f.found
}

/// An aggregate the respelling can take apart, and what it reads.
#[derive(Debug, Clone)]
pub enum Aggregate {
    CountRows,
    Count(Expr),
    CountDistinct(Vec<Expr>),
    Sum(Expr),
    Min(Expr),
    Max(Expr),
}

/// The aggregate a function call is, when the respelling can take it apart.
pub fn aggregate(f: &sqlparser::ast::Function) -> Option<Aggregate> {
    if f.filter.is_some()
        || f.null_treatment.is_some()
        || !f.within_group.is_empty()
        || !matches!(f.parameters, FunctionArguments::None)
        || f.over.is_some()
    {
        return None;
    }
    let FunctionArguments::List(list) = &f.args else {
        return None;
    };
    if !list.clauses.is_empty() {
        return None;
    }
    let distinct = matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct));
    let exprs: Option<Vec<Expr>> = list
        .args
        .iter()
        .map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
            _ => None,
        })
        .collect();
    let one = |exprs: Option<Vec<Expr>>| match exprs.as_deref() {
        Some([e]) => Some(e.clone()),
        _ => None,
    };
    match function_name(f)?.as_str() {
        "COUNT" => match list.args.as_slice() {
            [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)] if !distinct => {
                Some(Aggregate::CountRows)
            }
            _ if distinct => Some(Aggregate::CountDistinct(exprs.filter(|e| !e.is_empty())?)),
            _ => Some(Aggregate::Count(one(exprs)?)),
        },
        "SUM" if !distinct => Some(Aggregate::Sum(one(exprs)?)),
        "MIN" => Some(Aggregate::Min(one(exprs)?)),
        "MAX" => Some(Aggregate::Max(one(exprs)?)),
        _ => None,
    }
}

/// Whether, from each row of `grain`, the joins within `members` reach at most one row of every
/// other member: each is reached through equalities on all the columns of one of its unique keys,
/// with a column of a member already reached, of the same kind, or with a constant of that kind.
pub fn to_one(rels: &[Rel], conds: &[Expr], grain: usize, members: &[usize]) -> bool {
    let mut reached = vec![grain];
    loop {
        let before = reached.len();
        for &r in members {
            if reached.contains(&r) {
                continue;
            }
            let pinned: BTreeSet<usize> = conds
                .iter()
                .filter_map(|c| {
                    let Expr::BinaryOp {
                        left,
                        op: BinaryOperator::Eq,
                        right,
                    } = c
                    else {
                        return None;
                    };
                    for (this, that) in [(left, right), (right, left)] {
                        let Some((tr, tc)) = column_at(this, rels) else {
                            continue;
                        };
                        if tr != r {
                            continue;
                        }
                        let kind = rels[r].shape.columns[tc].kind.as_ref()?;
                        let fixed = match column_at(that, rels) {
                            Some((or, oc)) => {
                                reached.contains(&or)
                                    && rels[or].shape.columns[oc].kind.as_ref() == Some(kind)
                            }
                            None => constant_of(that, kind),
                        };
                        if fixed {
                            return Some(tc);
                        }
                    }
                    None
                })
                .collect();
            if rels[r].shape.unique.iter().any(|k| k.is_subset(&pinned)) {
                reached.push(r);
            }
        }
        if reached.len() == before {
            break;
        }
    }
    members.iter().all(|m| reached.contains(m))
}

/// Whether an expression is a constant compared with a column of `kind` as that kind: a number
/// with a number, a quoted string with text.
fn constant_of(e: &Expr, kind: &Kind) -> bool {
    matches!(
        (e, kind),
        (Expr::Value(v), Kind::Integer | Kind::Decimal) if matches!(v.value, Value::Number(..))
    ) || matches!(
        (e, kind),
        (Expr::Value(v), Kind::Text(_))
            if matches!(v.value, Value::SingleQuotedString(_) | Value::DoubleQuotedString(_))
    )
}

/// Whether a query reads a table of the given name, at any depth.
pub fn reads_table(q: &Query, table: &str) -> bool {
    struct Reads<'a> {
        table: &'a str,
        found: bool,
    }
    impl Visitor for Reads<'_> {
        type Break = ();
        fn pre_visit_table_factor(&mut self, tf: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table { name, .. } = tf {
                if let [part] = name.0.as_slice() {
                    if part.as_ident().is_some_and(|i| i.value == self.table) {
                        self.found = true;
                    }
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut r = Reads {
        table,
        found: false,
    };
    let _ = q.visit(&mut r);
    r.found
}

/// The number of FROM items anywhere in a statement that name `table` alone.
pub fn times_read<V: Visit + ?Sized>(node: &V, table: &str) -> usize {
    struct Count<'a> {
        table: &'a str,
        n: usize,
    }
    impl Visitor for Count<'_> {
        type Break = ();
        fn pre_visit_table_factor(&mut self, tf: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table { name, .. } = tf {
                if let [part] = name.0.as_slice() {
                    if part.as_ident().is_some_and(|i| i.value == self.table) {
                        self.n += 1;
                    }
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut c = Count { table, n: 0 };
    let _ = node.visit(&mut c);
    c.n
}

/// MySQL's functions whose value may differ between two calls in one statement.
const VOLATILE: [&str; 13] = [
    "RAND",
    "RANDOM_BYTES",
    "UUID",
    "UUID_SHORT",
    "SYSDATE",
    "SLEEP",
    "BENCHMARK",
    "GET_LOCK",
    "RELEASE_LOCK",
    "RELEASE_ALL_LOCKS",
    "IS_FREE_LOCK",
    "IS_USED_LOCK",
    "LAST_INSERT_ID",
];

/// What a part of a statement holds that a respelling must not copy, move or repeat: a `?`
/// parameter, a function whose value may change between calls, a user variable, a window function,
/// or a query nested in an expression.
#[derive(Default, Debug)]
pub struct Scan {
    pub placeholder: bool,
    pub volatile: bool,
    pub window: bool,
    pub subquery: bool,
}

impl Scan {
    /// Whether the part holds nothing a respelling must leave where it stands.
    pub fn movable(&self) -> bool {
        !(self.placeholder || self.volatile || self.window)
    }
}

impl Visitor for Scan {
    type Break = ();

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        match e {
            Expr::Value(v) if matches!(v.value, Value::Placeholder(_)) => self.placeholder = true,
            Expr::Function(f) => {
                if f.over.is_some() {
                    self.window = true;
                }
                if function_name(f).is_some_and(|c| VOLATILE.contains(&c.as_str())) {
                    self.volatile = true;
                }
            }
            // a user variable
            Expr::Identifier(i) if i.value.starts_with('@') => self.volatile = true,
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                self.subquery = true
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

pub fn scan<V: Visit + ?Sized>(node: &V) -> Scan {
    let mut s = Scan::default();
    let _ = node.visit(&mut s);
    s
}

/// Whether a SELECT is plain: no DISTINCT, no hint, no INTO, and nothing but a select list, a FROM,
/// a WHERE and, when `grouped`, a GROUP BY without modifiers.
pub fn plain(s: &Select, grouped: bool) -> bool {
    let groups = match &s.group_by {
        GroupByExpr::Expressions(items, modifiers) => {
            modifiers.is_empty() && (grouped || items.is_empty())
        }
        GroupByExpr::All(_) => false,
    };
    groups
        && s.optimizer_hints.is_empty()
        && s.distinct.is_none()
        && s.select_modifiers.is_none()
        && s.top.is_none()
        && s.exclude.is_none()
        && s.into.is_none()
        && s.lateral_views.is_empty()
        && s.prewhere.is_none()
        && s.connect_by.is_empty()
        && s.cluster_by.is_empty()
        && s.distribute_by.is_empty()
        && s.sort_by.is_empty()
        && s.having.is_none()
        && s.named_window.is_empty()
        && s.qualify.is_none()
        && s.value_table_mode.is_none()
        && s.flavor == SelectFlavor::Standard
}

/// Whether a query has nothing around its body: no ORDER BY, LIMIT, FETCH, locking or settings.
pub fn bare(q: &Query) -> bool {
    q.order_by.is_none()
        && q.limit_clause.is_none()
        && q.fetch.is_none()
        && q.locks.is_empty()
        && q.for_clause.is_none()
        && q.settings.is_none()
        && q.format_clause.is_none()
        && q.pipe_operators.is_empty()
}

/// Every column reference of an expression written as `relation.column`, by the relation's
/// qualifier and the column's own name; none when a reference names no column of `rels`, or names
/// one ambiguously. A query nested in the expression is left as written.
pub fn qualified(e: &Expr, rels: &[Rel]) -> Option<Expr> {
    struct Qualify<'a> {
        rels: &'a [Rel],
        depth: usize,
        failed: bool,
    }
    impl VisitorMut for Qualify<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
            if self.depth > 0 {
                return ControlFlow::Continue(());
            }
            if let Expr::Identifier(_) | Expr::CompoundIdentifier(_) = e {
                match column_at(e, self.rels) {
                    Some((r, c)) => *e = reference(&self.rels[r], c, e),
                    None => self.failed = true,
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let mut q = Qualify {
        rels,
        depth: 0,
        failed: false,
    };
    let _ = VisitMut::visit(&mut e, &mut q);
    (!q.failed).then_some(e)
}

/// A reference to a relation's column, `qualifier.column`, keeping the quoting the statement gave
/// the column.
fn reference(rel: &Rel, column: usize, written: &Expr) -> Expr {
    let name = rel.shape.columns[column].name.clone().unwrap_or_default();
    let quoted = match written {
        Expr::Identifier(i) => i.quote_style.is_some(),
        Expr::CompoundIdentifier(p) => p.last().is_some_and(|i| i.quote_style.is_some()),
        _ => false,
    };
    let bare = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let col = if quoted || !bare {
        Ident::with_quote('`', name)
    } else {
        Ident::new(name)
    };
    Expr::CompoundIdentifier(vec![rel.qualifier.clone(), col])
}

/// The relations an expression's column references name, by their position in `rels`; the
/// expression must already be qualified.
pub fn relations_of(e: &Expr, rels: &[Rel]) -> BTreeSet<usize> {
    struct Find<'a> {
        rels: &'a [Rel],
        found: BTreeSet<usize>,
    }
    impl Visitor for Find<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            if let Some((r, _)) = column_at(e, self.rels) {
                self.found.insert(r);
            }
            ControlFlow::Continue(())
        }
    }
    let mut f = Find {
        rels,
        found: BTreeSet::new(),
    };
    let _ = e.visit(&mut f);
    f.found
}

/// Every name a statement uses, in lower case: identifiers of every kind.
pub fn names_used<V: Visit + ?Sized>(node: &V) -> BTreeSet<String> {
    struct Names(BTreeSet<String>);
    impl Visitor for Names {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            match e {
                Expr::Identifier(i) => {
                    self.0.insert(i.value.to_lowercase());
                }
                Expr::CompoundIdentifier(p) => {
                    for i in p {
                        self.0.insert(i.value.to_lowercase());
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_relation(&mut self, n: &ObjectName) -> ControlFlow<()> {
            for p in &n.0 {
                if let Some(i) = p.as_ident() {
                    self.0.insert(i.value.to_lowercase());
                }
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_table_factor(&mut self, tf: &TableFactor) -> ControlFlow<()> {
            let alias = match tf {
                TableFactor::Table { alias, .. } | TableFactor::Derived { alias, .. } => {
                    alias.as_ref()
                }
                _ => None,
            };
            if let Some(a) = alias {
                self.0.insert(a.name.value.to_lowercase());
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            if let Some(w) = &q.with {
                for c in &w.cte_tables {
                    self.0.insert(c.alias.name.value.to_lowercase());
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut n = Names(BTreeSet::new());
    let _ = node.visit(&mut n);
    n.0
}

/// A name for something the respelling adds that the statement does not use: `base`, or `base`
/// with a number after it.
pub fn fresh(base: &str, used: &mut BTreeSet<String>) -> String {
    let mut name = base.to_string();
    let mut n = 1;
    while used.contains(&name) {
        n += 1;
        name = format!("{base}{n}");
    }
    used.insert(name.clone());
    name
}

/// Whether some of `rels` are joined to each other by the conditions: each reached from the first
/// through conditions that read it and one already reached.
pub fn connected(members: &[usize], conds: &[Expr], rels: &[Rel]) -> bool {
    let Some(&start) = members.first() else {
        return false;
    };
    let mut reached = vec![start];
    loop {
        let before = reached.len();
        for c in conds {
            let on = relations_of(c, rels);
            if on.iter().any(|r| reached.contains(r)) {
                for r in on {
                    if !reached.contains(&r) {
                        reached.push(r);
                    }
                }
            }
        }
        if reached.len() == before {
            break;
        }
    }
    members.iter().all(|m| reached.contains(m))
}

/// The names of the tables and WITH queries a part of a statement reads by one name alone, at any
/// depth.
pub fn tables_read<V: Visit + ?Sized>(node: &V) -> BTreeSet<String> {
    struct Tables(BTreeSet<String>);
    impl Visitor for Tables {
        type Break = ();
        fn pre_visit_table_factor(&mut self, tf: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table { name, .. } = tf {
                if let [part] = name.0.as_slice() {
                    if let Some(i) = part.as_ident() {
                        self.0.insert(i.value.clone());
                    }
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut t = Tables(BTreeSet::new());
    let _ = node.visit(&mut t);
    t.0
}
