// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a statement's text in a dialect sqlparser reads, with string literals kept exactly as
//! written, so that a statement written back reads the same on the server; and writing a name.

use sqlparser::ast::{Expr, Ident, Query, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::keywords::ALL_KEYWORDS;
use sqlparser::parser::{Parser, ParserError, ParserOptions};
use sqlparser::tokenizer::Token;

fn options() -> ParserOptions {
    ParserOptions::new().with_unescape(false)
}

/// The statements of a text.
pub fn parse_statements(dialect: &dyn Dialect, text: &str) -> Result<Vec<Statement>, ParserError> {
    Parser::new(dialect)
        .with_options(options())
        .try_with_sql(text)?
        .parse_statements()
}

/// A text that is one query, and nothing more.
pub fn parse_query(dialect: &dyn Dialect, text: &str) -> Option<Query> {
    let mut statements = parse_statements(dialect, text).ok()?;
    if statements.len() != 1 {
        return None;
    }
    match statements.pop()? {
        Statement::Query(q) => Some(*q),
        _ => None,
    }
}

/// A text that is one expression, and nothing more.
pub fn parse_expr(dialect: &dyn Dialect, text: &str) -> Option<Expr> {
    let mut parser = Parser::new(dialect)
        .with_options(options())
        .try_with_sql(text)
        .ok()?;
    let expr = parser.parse_expr().ok()?;
    (parser.peek_token().token == Token::EOF).then_some(expr)
}

/// An identifier for a name a respelling writes: plain when it can be, in `quote` otherwise.
pub fn ident(name: &str, quote: char) -> Ident {
    let plain = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let upper = name.to_ascii_uppercase();
    if plain && !ALL_KEYWORDS.contains(&upper.as_str()) {
        Ident::new(name)
    } else {
        Ident::with_quote(quote, name)
    }
}
