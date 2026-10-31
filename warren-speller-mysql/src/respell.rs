// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The respelling: a statement rewritten against a database's catalog. A statement nothing applies
//! to is written back exactly as it was read.
//!
//! - **A relation every UNION ALL arm joins** is joined once, to the union of the rest of each arm
//!   (`union`).
//! - **A grouped SELECT over a join that splits in two** is grouped on each side before the join,
//!   the side not holding the rows a count counts only over the joined values the other holds
//!   (`fold`).
//!
//! Both apply to the statement, its WITH queries and its derived tables, from the innermost out,
//! whatever keys the tables have. A `?` parameter is never copied or moved: the respelled statement
//! binds the same parameters in the same order as the one written.

mod fold;
mod relations;
mod union;

use crate::catalog::Catalog;
use crate::{sql, Refused};
use relations::{relations_of, Ctx, Rel};
use sqlparser::ast::{
    BinaryOperator, Expr, Query, SetExpr, Statement, TableFactor, TableWithJoins,
};
use std::collections::BTreeSet;

/// One statement respelled against the catalog, followed by a line break; the statement exactly as
/// read when nothing applies to it. Anything but a query is written back as read.
pub fn respell(catalog: &Catalog, statement: &str) -> Result<String, Refused> {
    respell_with(catalog, statement, |_| {})
}

/// One statement respelled against the catalog as [`respell`] does, with the caller's own change
/// made to the query first. The statement is written back exactly as read when neither changes it.
pub fn respell_with(
    catalog: &Catalog,
    statement: &str,
    first: impl FnOnce(&mut Query),
) -> Result<String, Refused> {
    let mut statements = sql::parse_statements(statement)
        .map_err(|e| Refused(format!("the statement could not be read: {e}")))?;
    if statements.len() != 1 {
        return Err(Refused(format!(
            "{} statements were read; respell reads one",
            statements.len()
        )));
    }
    let Some(Statement::Query(mut q)) = statements.pop() else {
        return Ok(statement.to_string());
    };
    let before = q.clone();
    first(&mut q);
    respell_query(catalog, &mut q);
    if q == before {
        return Ok(statement.to_string());
    }
    let end = if statement.trim_end().ends_with(';') {
        ";"
    } else {
        ""
    };
    Ok(format!("{q}{end}\n"))
}

/// A query respelled in place against the catalog; left as it is when nothing applies to it.
pub fn respell_query(catalog: &Catalog, q: &mut Query) {
    let mut used = relations::names_used(&*q);
    identities(q, &Ctx::new(catalog), &mut used);
}

// ---------------------------------------------------------------------------------------------
// The identities, from the innermost query out

/// The join taken out of a union, and the grouping taken to each side of a cut, in a query, its
/// WITH queries and its derived tables. A recursive WITH query is left as written.
fn identities(q: &mut Query, ctx: &Ctx, used: &mut BTreeSet<String>) {
    if let Some(with) = &q.with {
        let recursive = with.recursive;
        for i in 0..with.cte_tables.len() {
            let mut before = q.clone();
            if let Some(w) = &mut before.with {
                w.cte_tables.truncate(i);
            }
            let scope = ctx.inside(&before);
            let Some(with) = &mut q.with else { return };
            let cte = &mut with.cte_tables[i];
            if !(recursive && relations::reads_table(&cte.query, &cte.alias.name.value)) {
                identities(&mut cte.query, &scope, used);
            }
        }
    }
    let inner = ctx.inside(q);
    derived(&mut q.body, &inner, used);
    if let Some(out) = union::distribute(q, ctx, used) {
        *q = out;
    }
    if let Some(out) = fold::fold(q, ctx, used) {
        *q = out;
    }
}

/// The identities in each derived table a query's body reads that is not LATERAL.
fn derived(body: &mut SetExpr, ctx: &Ctx, used: &mut BTreeSet<String>) {
    fn factor(tf: &mut TableFactor, ctx: &Ctx, used: &mut BTreeSet<String>) {
        match tf {
            TableFactor::Derived {
                lateral: false,
                subquery,
                ..
            } => identities(subquery, ctx, used),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => joins(table_with_joins, ctx, used),
            _ => {}
        }
    }
    fn joins(twj: &mut TableWithJoins, ctx: &Ctx, used: &mut BTreeSet<String>) {
        factor(&mut twj.relation, ctx, used);
        for j in &mut twj.joins {
            factor(&mut j.relation, ctx, used);
        }
    }
    match body {
        SetExpr::Select(s) => {
            for twj in &mut s.from {
                joins(twj, ctx, used);
            }
        }
        SetExpr::Query(q) => identities(q, ctx, used),
        SetExpr::SetOperation { left, right, .. } => {
            derived(left, ctx, used);
            derived(right, ctx, used);
        }
        _ => {}
    }
}

/// A FROM over some of `rels`, in their order, each joined ON the conditions it completes; and the
/// conditions that read the first alone, or no relation, for a WHERE. None when a condition reads
/// a relation outside them.
fn from_text(rels: &[Rel], members: &[usize], conds: &[Expr]) -> Option<(String, Vec<Expr>)> {
    let mut on: Vec<Vec<Expr>> = vec![Vec::new(); members.len()];
    let mut filter = Vec::new();
    for c in conds {
        let mut last = 0;
        for r in relations_of(c, rels) {
            last = last.max(members.iter().position(|&m| m == r)?);
        }
        if last == 0 {
            filter.push(c.clone());
        } else {
            on[last].push(c.clone());
        }
    }
    let mut text = rels[*members.first()?].factor.to_string();
    for (i, &m) in members.iter().enumerate().skip(1) {
        if on[i].is_empty() {
            text.push_str(&format!(" CROSS JOIN {}", rels[m].factor));
        } else {
            text.push_str(&format!(" JOIN {} ON {}", rels[m].factor, and_text(&on[i])));
        }
    }
    Some((text, filter))
}

/// Conditions ANDed, each that binds more loosely than AND in parentheses.
fn and_text(conds: &[Expr]) -> String {
    conds
        .iter()
        .map(|c| match c {
            Expr::BinaryOp {
                op: BinaryOperator::Or | BinaryOperator::Xor | BinaryOperator::StringConcat,
                ..
            } => {
                format!("({c})")
            }
            _ => c.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}
