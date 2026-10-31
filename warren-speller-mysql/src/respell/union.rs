// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A relation joined in every arm of a UNION ALL, on the same condition, is joined once to the union
//! of the rest of each arm.
//!
//! The union's arms are plain SELECTs over inner joins. One table or WITH query is read in every arm,
//! once, and joined there by equalities of its columns with the rest of the arm; the columns and
//! the conditions on it alone are the same in every arm, and each column the union returns is read
//! from it alone in every arm or from the rest alone. The respelled query reads it once, joined to
//! a derived table holding the union of the rest of each arm and the values it was joined on.

use super::relations::{
    bare, column_at, conditions, inner_joins_only, plain, qualified, relations_of, scan, Ctx, Rel,
    What,
};
use super::{and_text, from_text};
use crate::sql;
use sqlparser::ast::{
    BinaryOperator, Expr, Query, Select, SelectItem, SetExpr, SetOperator, SetQuantifier,
    TableFactor,
};
use std::collections::BTreeSet;

/// One arm of the union, read.
struct Arm {
    rels: Vec<Rel>,
    /// The relation read in every arm.
    common: usize,
    /// The equalities joining it: its column, and what the rest of the arm gives it.
    joined: Vec<(usize, Expr)>,
    /// The conditions on it alone, written with the first arm's name for it.
    own: Vec<Expr>,
    /// The conditions on the rest of the arm.
    rest: Vec<Expr>,
    /// Each column the arm returns, qualified.
    items: Vec<Expr>,
}

/// The query with the relation every arm joins taken out of its union; none where it does not
/// apply.
pub fn distribute(q: &Query, ctx: &Ctx, used: &mut BTreeSet<String>) -> Option<Query> {
    if !bare(q) {
        return None;
    }
    let mut selects = Vec::new();
    if !arms(&q.body, &mut selects) || selects.len() < 2 {
        return None;
    }
    let ctx = ctx.inside(q);
    for s in &selects {
        let sc = scan(*s);
        if !plain(s, false) || !inner_joins_only(&s.from) || !sc.movable() || sc.subquery {
            return None;
        }
        if s.projection.iter().any(|i| {
            !matches!(i, SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } if !super::relations::has_aggregate(e))
        }) {
            return None;
        }
    }
    let all: Vec<Vec<Rel>> = selects
        .iter()
        .map(|s| ctx.relations(&s.from))
        .collect::<Option<_>>()?;
    // the relations of the first arm that every arm reads once by the same name
    for (first, candidate) in all[0].iter().enumerate() {
        if matches!(candidate.what, What::Derived) {
            continue;
        }
        let common: Option<Vec<usize>> = all
            .iter()
            .map(|rels| {
                let same: Vec<usize> = rels
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.what == candidate.what
                            && unaliased(&r.factor) == unaliased(&candidate.factor)
                    })
                    .map(|(i, _)| i)
                    .collect();
                match same.as_slice() {
                    [one] => Some(*one),
                    _ => None,
                }
            })
            .collect();
        let Some(common) = common else { continue };
        let arms: Option<Vec<Arm>> = selects
            .iter()
            .zip(all.iter().cloned())
            .zip(&common)
            .map(|((s, rels), &c)| read_arm(s, rels, c, &all[0][first]))
            .collect();
        if let Some(arms) = arms {
            if let Some(out) = respelled(q, &selects, &arms, used) {
                return Some(out);
            }
        }
    }
    None
}

/// The SELECTs of a union of UNION ALL arms, in order.
fn arms<'a>(body: &'a SetExpr, out: &mut Vec<&'a Select>) -> bool {
    match body {
        SetExpr::SetOperation {
            left,
            op: SetOperator::Union,
            set_quantifier: SetQuantifier::All,
            right,
        } => arms(left, out) && arms(right, out),
        SetExpr::Select(s) => {
            out.push(s);
            true
        }
        _ => false,
    }
}

/// A FROM item as written, without its alias.
fn unaliased(tf: &TableFactor) -> String {
    let mut tf = tf.clone();
    if let TableFactor::Table { alias, .. } = &mut tf {
        *alias = None;
    }
    tf.to_string()
}

/// An expression with every reference to one relation's columns qualified by another name.
fn renamed(e: &Expr, from: &str, to: &sqlparser::ast::Ident) -> Expr {
    use sqlparser::ast::VisitorMut;
    struct Rename<'a> {
        from: &'a str,
        to: &'a sqlparser::ast::Ident,
    }
    impl VisitorMut for Rename<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &mut Expr) -> std::ops::ControlFlow<()> {
            if let Expr::CompoundIdentifier(p) = e {
                if p.len() == 2 && p[0].value == self.from {
                    p[0] = self.to.clone();
                }
            }
            std::ops::ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let _ = sqlparser::ast::VisitMut::visit(&mut e, &mut Rename { from, to });
    e
}

fn read_arm(s: &Select, rels: Vec<Rel>, common: usize, first: &Rel) -> Option<Arm> {
    let own_name = rels[common].qualifier.value.clone();
    let mut joined = Vec::new();
    let mut own = Vec::new();
    let mut rest = Vec::new();
    for c in conditions(s) {
        let c = qualified(&c, &rels)?;
        let on = relations_of(&c, &rels);
        if !on.contains(&common) {
            rest.push(c);
        } else if on.len() == 1 {
            own.push(renamed(&c, &own_name, &first.qualifier));
        } else {
            let Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } = &c
            else {
                return None;
            };
            let (column, other) = match (column_at(left, &rels), column_at(right, &rels)) {
                (Some((r, col)), _)
                    if r == common && !relations_of(right, &rels).contains(&common) =>
                {
                    (col, right.as_ref().clone())
                }
                (_, Some((r, col)))
                    if r == common && !relations_of(left, &rels).contains(&common) =>
                {
                    (col, left.as_ref().clone())
                }
                _ => return None,
            };
            joined.push((column, other));
        }
    }
    if joined.is_empty() || rels.len() < 2 {
        return None;
    }
    joined.sort_by_key(|(c, e)| (*c, e.to_string()));
    own.sort_by_key(|e| e.to_string());
    let items = s
        .projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                qualified(e, &rels)
            }
            _ => None,
        })
        .collect::<Option<Vec<Expr>>>()?;
    Some(Arm {
        rels,
        common,
        joined,
        own,
        rest,
        items,
    })
}

fn respelled(
    q: &Query,
    selects: &[&Select],
    arms: &[Arm],
    used: &mut BTreeSet<String>,
) -> Option<Query> {
    let first = &arms[0];
    let columns: Vec<usize> = first.joined.iter().map(|(c, _)| *c).collect();
    let width = first.items.len();
    for a in arms {
        if a.joined.iter().map(|(c, _)| *c).collect::<Vec<_>>() != columns
            || a.own
                .iter()
                .map(|e| e.to_string())
                .ne(first.own.iter().map(|e| e.to_string()))
            || a.items.len() != width
        {
            return None;
        }
    }
    // each column returned: read from the common relation alone in every arm, or from the rest
    let mut from_common = Vec::new();
    for j in 0..width {
        let mut sides = arms.iter().map(|a| {
            let on = relations_of(&a.items[j], &a.rels);
            if on.contains(&a.common) {
                (on.len() == 1).then(|| {
                    renamed(
                        &a.items[j],
                        &a.rels[a.common].qualifier.value,
                        &first.rels[first.common].qualifier,
                    )
                    .to_string()
                })
            } else {
                Some(String::new())
            }
        });
        let one = sides.next()??;
        for other in sides {
            if other? != one {
                return None;
            }
        }
        from_common.push(!one.is_empty());
    }
    // the names the union gives its columns: the first arm's
    let names: Vec<sqlparser::ast::Ident> = selects[0]
        .projection
        .iter()
        .map(|i| match i {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
            SelectItem::UnnamedExpr(Expr::Identifier(i)) => Some(i.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(p)) => p.last().cloned(),
            _ => None,
        })
        .collect::<Option<_>>()?;

    let union_name = super::relations::fresh("arms", used);
    let alias = sql::ident(&union_name);
    let keys: Vec<String> = (1..=columns.len()).map(|i| format!("k{i}")).collect();
    let rest_columns: Vec<usize> = (0..width).filter(|&j| !from_common[j]).collect();
    let values: Vec<String> = (1..=rest_columns.len()).map(|i| format!("c{i}")).collect();
    let mut branches = Vec::new();
    for a in arms {
        let others: Vec<usize> = (0..a.rels.len()).filter(|&r| r != a.common).collect();
        if !super::relations::connected(&others, &a.rest, &a.rels) {
            return None;
        }
        let (from, filter) = from_text(&a.rels, &others, &a.rest)?;
        let mut items: Vec<String> = a.joined.iter().map(|(_, e)| e.to_string()).collect();
        items.extend(rest_columns.iter().map(|&j| a.items[j].to_string()));
        let mut text = format!("SELECT {} FROM {from}", items.join(", "));
        if !filter.is_empty() {
            text.push_str(&format!(" WHERE {}", and_text(&filter)));
        }
        branches.push(text);
    }
    let common = &first.rels[first.common];
    let mut items = Vec::new();
    let mut next = 0;
    for j in 0..width {
        let name = &names[j];
        if from_common[j] {
            let e = &first.items[j];
            let named = matches!(e, Expr::CompoundIdentifier(p) if p.last().is_some_and(|i| i.value == name.value));
            items.push(if named {
                e.to_string()
            } else {
                format!("{e} AS {name}")
            });
        } else {
            items.push(format!("{alias}.{} AS {name}", values[next]));
            next += 1;
        }
    }
    let on: Vec<String> = first
        .joined
        .iter()
        .zip(&keys)
        .map(|((c, _), k)| {
            let col = common.shape.columns[*c].name.clone().unwrap_or_default();
            format!("{}.{} = {alias}.{k}", common.qualifier, sql::ident(&col))
        })
        .collect();
    let mut list = keys.clone();
    list.extend(values);
    let mut text = format!(
        "SELECT {} FROM {} JOIN ({}) AS {alias} ({}) ON {}",
        items.join(", "),
        common.factor,
        branches.join(" UNION ALL "),
        list.join(", "),
        on.join(" AND ")
    );
    if !first.own.is_empty() {
        text.push_str(&format!(" WHERE {}", and_text(&first.own)));
    }
    let parsed = sql::parse_query(&text)?;
    let mut out = q.clone();
    out.body = parsed.body;
    Some(out)
}
