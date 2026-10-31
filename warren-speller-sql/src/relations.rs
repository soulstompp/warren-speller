// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a SELECT reads: each table, WITH query or derived table in its FROM with the columns it
//! returns (their names, how each compares, whether each may hold NULL) and its unique keys; the
//! conditions of its WHERE and its joins' ONs, as one list; and every column reference written as
//! `relation.column`.
//!
//! How a column compares is the caller's own type, `K`, as the caller's catalog tells it.

use sqlparser::ast::{
    BinaryOperator, DuplicateTreatment, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Ident, JoinConstraint, JoinOperator, ObjectName, Query, Select, SelectFlavor,
    TableFactor, TableWithJoins, Visit, VisitMut, Visitor, VisitorMut,
};
use std::collections::BTreeSet;
use std::ops::ControlFlow;

/// A column a relation returns.
#[derive(Debug, Clone)]
pub struct Out<K> {
    /// Its name; none where the server would name it by the text of its expression.
    pub name: Option<String>,
    pub not_null: bool,
    /// How it compares; none where that cannot be told.
    pub kind: Option<K>,
}

/// The columns a relation returns, and its unique keys, each a set of positions among them whose
/// values hold no NULL.
#[derive(Debug, Clone, Default)]
pub struct Shape<K> {
    pub columns: Vec<Out<K>>,
    pub unique: Vec<BTreeSet<usize>>,
}

impl<K> Shape<K> {
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
    pub fn renamed(mut self, names: &[Ident]) -> Option<Shape<K>> {
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

/// Where a relation's rows come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    Table(String),
    Cte(String),
    Derived,
}

/// A relation a SELECT reads.
#[derive(Debug, Clone)]
pub struct Rel<K> {
    /// The FROM item as written.
    pub factor: TableFactor,
    /// The name that qualifies its columns: its alias, or its table's name.
    pub qualifier: Ident,
    /// Whether it is read under an alias.
    pub aliased: bool,
    /// The table's name as written; empty for a derived table.
    pub written: Vec<Ident>,
    pub shape: Shape<K>,
    /// Whether `SELECT *` returns each column.
    pub listed: Vec<bool>,
    pub what: What,
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
pub fn column_at<K>(e: &Expr, rels: &[Rel<K>]) -> Option<(usize, usize)> {
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

/// A function's name, in capitals, when it is one plain name.
pub fn function_name(f: &sqlparser::ast::Function) -> Option<String> {
    let [part] = f.name.0.as_slice() else {
        return None;
    };
    Some(part.as_ident()?.value.to_ascii_uppercase())
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
/// qualifier and the column's own name, in `quote` where it is quoted; none when a reference names
/// no column of `rels`, or names one ambiguously. A query nested in the expression is left as
/// written.
pub fn qualified<K>(e: &Expr, rels: &[Rel<K>], quote: char) -> Option<Expr> {
    struct Qualify<'a, K> {
        rels: &'a [Rel<K>],
        quote: char,
        depth: usize,
        failed: bool,
    }
    impl<K> VisitorMut for Qualify<'_, K> {
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
                    Some((r, c)) => *e = reference(&self.rels[r], c, e, self.quote),
                    None => self.failed = true,
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let mut q = Qualify {
        rels,
        quote,
        depth: 0,
        failed: false,
    };
    let _ = VisitMut::visit(&mut e, &mut q);
    (!q.failed).then_some(e)
}

/// A reference to a relation's column, `qualifier.column`, keeping the quoting the statement gave
/// the column, in `quote`.
fn reference<K>(rel: &Rel<K>, column: usize, written: &Expr, quote: char) -> Expr {
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
        Ident::with_quote(quote, name)
    } else {
        Ident::new(name)
    };
    Expr::CompoundIdentifier(vec![rel.qualifier.clone(), col])
}

/// The relations an expression's column references name, by their position in `rels`; the
/// expression must already be qualified.
pub fn relations_of<K>(e: &Expr, rels: &[Rel<K>]) -> BTreeSet<usize> {
    struct Find<'a, K> {
        rels: &'a [Rel<K>],
        found: BTreeSet<usize>,
    }
    impl<K> Visitor for Find<'_, K> {
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
pub fn connected<K>(members: &[usize], conds: &[Expr], rels: &[Rel<K>]) -> bool {
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
