// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a SELECT reads, read against the catalog: each table, WITH query or derived table in its
//! FROM with the columns it returns (their names, the type each compares as, whether each may hold
//! NULL) and its unique keys; and what MySQL's functions and types tell of an expression: its type,
//! whether it is an aggregate, whether its value may change between calls. The statement's
//! relations, joins and conditions are read on sqlparser's tree by warren-speller-sql.

use crate::catalog::{Catalog, Kind};
use crate::sql;
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, GroupByExpr, Ident, ObjectName, Query, Select,
    SelectItem, SetExpr, SetOperator, SetQuantifier, TableFactor, TableWithJoins, Value, Visit,
    Visitor,
};
use std::collections::BTreeSet;
use std::ops::ControlFlow;
use warren_speller_sql::relations::function_name;
pub use warren_speller_sql::relations::{
    aggregate, bare, column_at, conditions, connected, fresh, inner_joins_only, names_used, plain,
    reads_table, relations_of, tables_read, times_read, Aggregate, What,
};

/// A column a relation returns, compared as MySQL compares it.
pub type Out = warren_speller_sql::relations::Out<Kind>;

/// The columns a relation returns, and its unique keys, compared as MySQL compares them.
pub type Shape = warren_speller_sql::relations::Shape<Kind>;

/// A relation a SELECT reads, its columns compared as MySQL compares them.
pub type Rel = warren_speller_sql::relations::Rel<Kind>;

/// Every column reference of an expression written as `relation.column`, in backquotes where it is
/// quoted; none when a reference names no column of `rels`, or names one ambiguously.
pub fn qualified(e: &Expr, rels: &[Rel]) -> Option<Expr> {
    warren_speller_sql::relations::qualified(e, rels, sql::QUOTE)
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

fn idents(name: &ObjectName) -> Option<Vec<Ident>> {
    name.0.iter().map(|p| p.as_ident().cloned()).collect()
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
/// or a query nested in an expression; and `||`, by which a statement is left as written: MySQL
/// reads it as OR or as concatenation by the session's sql_mode.
#[derive(Default, Debug)]
pub struct Scan {
    pub placeholder: bool,
    pub volatile: bool,
    pub window: bool,
    pub subquery: bool,
    pub bars: bool,
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
            Expr::BinaryOp {
                op: BinaryOperator::StringConcat,
                ..
            } => self.bars = true,
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
