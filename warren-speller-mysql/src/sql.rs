// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading MySQL statements: sqlparser's MySQL dialect, with string literals kept exactly as written,
//! so that a statement written back reads the same on the server; and writing a name.

use sqlparser::ast::{Expr, Ident, Query, Statement};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::ParserError;
use sqlparser::tokenizer::{Token, Tokenizer};

/// The quote MySQL writes a name in.
pub(crate) const QUOTE: char = '`';

/// Whether a text holds `||` or `&&`, which the parser reads otherwise than MySQL may: MySQL reads
/// `||` as OR unless the session's sql_mode has PIPES_AS_CONCAT, and `&&` as AND, binding as
/// loosely as AND does.
pub fn holds_bars_or_ampersands(text: &str) -> bool {
    Tokenizer::new(&MySqlDialect {}, text)
        .with_unescape(false)
        .tokenize()
        .map_or(true, |tokens| {
            tokens
                .iter()
                .any(|t| matches!(t, Token::StringConcat | Token::Overlap | Token::PGCubeRoot))
        })
}

/// The statements of a text.
pub fn parse_statements(text: &str) -> Result<Vec<Statement>, ParserError> {
    warren_speller_sql::text::parse_statements(&MySqlDialect {}, text)
}

/// A text that is one query, and nothing more.
pub fn parse_query(text: &str) -> Option<Query> {
    warren_speller_sql::text::parse_query(&MySqlDialect {}, text)
}

/// A text that is one expression, and nothing more.
pub fn parse_expr(text: &str) -> Option<Expr> {
    warren_speller_sql::text::parse_expr(&MySqlDialect {}, text)
}

/// An identifier for a name a respelling writes: plain when it can be, in backquotes otherwise.
pub fn ident(name: &str) -> Ident {
    warren_speller_sql::text::ident(name, QUOTE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_written_back_reads_as_the_statement_read() {
        for sql in [
            "WITH RECURSIVE r (a, b) AS (SELECT id, id FROM t WHERE p IS NULL UNION ALL \
             SELECT t.id, r.b FROM t JOIN r ON t.p = r.a) SELECT a FROM r",
            "SELECT COUNT(DISTINCT x.a) AS n, SUM(x.q * y.c) AS s FROM x JOIN y ON y.i = x.i AND y.v = 1 \
             GROUP BY 1 ORDER BY s DESC, 1 LIMIT 20",
            "SELECT CAST(SUM(a) AS SIGNED) AS s FROM t CROSS JOIN LATERAL (SELECT b FROM u WHERE u.k = t.k) AS x",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = t.a) \
             AND (MONTH(t.d) = 12 AND t.d >= '2010-01-01 00:00:00')",
            "SELECT * FROM (VALUES ROW(1, 'a'), ROW(2, 'b' COLLATE utf8mb4_0900_bin)) AS v (x, `value`)",
            r"SELECT 'it''s \\ \'q\'' AS s FROM t WHERE (true)",
            "SELECT a FROM t WHERE a = ? LIMIT ?",
        ] {
            let once = parse_statements(sql).unwrap();
            let text = once[0].to_string();
            assert_eq!(text, sql, "written back otherwise");
            assert_eq!(parse_statements(&text).unwrap(), once, "{text}");
        }
    }
}
