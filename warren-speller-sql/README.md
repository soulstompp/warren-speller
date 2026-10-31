<!-- Copyright (c) 2026 Kenneth Allen Flegal -->
<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# warren-speller-sql

*What a statement reads, read from its text.*

A speller respells a statement so that it reads less and returns the same rows. Before it can
write anything, it has to know what the statement reads: the tables, WITH queries and derived tables
each `SELECT` joins, the columns each returns and which of them are unique keys, how the joins are
made, and which relations each condition reads. `warren-speller-sql` reads all of that from the
syntax tree [sqlparser](https://crates.io/crates/sqlparser) builds, so that a speller spends its own
code on what it writes.

It is for whoever respells SQL text before a server plans it: in a program, a proxy or a driver.
[warren-speller-mysql][mysql] is built on it. It reads, and leaves the writing to the speller.

It is a library, licensed `MIT OR Apache-2.0`.

## What it reads

- **A statement's text,** in any dialect sqlparser reads, with its string literals kept exactly as
  written, so that a statement written back reads the same on the server.
- **The relations of a `SELECT`:** each table, WITH query or derived table of its `FROM`, by the
  name that qualifies its columns, with the columns it returns (their names, whether each may hold
  NULL, how each compares, whether `SELECT *` lists it) and its unique keys.
- **Its joins:** whether every join of a `FROM` is an inner join.
- **Its conditions:** the `WHERE` and every `ON`, split at each `AND`; the relations each condition
  reads; whether some of the relations are joined to one another by them; and every column
  reference of an expression written out as `relation.column`.
- **What else a speller asks of a statement:** the aggregates it can take apart (`COUNT`, `SUM`,
  `MIN` and `MAX`), whether a `SELECT` holds nothing but a select list, a `FROM`, a `WHERE` and a
  `GROUP BY`, whether a query has nothing around its body, the tables it reads and how many times,
  and every name it uses.
- **A name to write:** one the statement does not use, for something a speller adds; written plain
  where it can be, and in the dialect's quotes otherwise.

## What it leaves to the speller

Each engine answers these its own way, so each speller brings its own answers:

- **The catalog:** what a database says of its tables, and how a column's values compare. A speller
  reads its catalog into the relations, and how a column compares is the speller's own type.
- **The dialect's functions:** which are aggregates, and which may give another value on a second
  call.
- **The dialect:** the one a text is read in, and the quote a name is written in.
- **What to write** in place of the statement.

## Using it

- `text::parse_statements(dialect, text)`, `text::parse_query` and `text::parse_expr` read a text
  as statements, one query or one expression. `text::ident(name, quote)` writes a name.
- `relations::Rel<K>` is a relation a `SELECT` reads, `K` how the speller's catalog says a column
  compares. `relations::conditions`, `relations::inner_joins_only` and the functions beside them
  read a `SELECT` and its relations.
- The crate re-exports sqlparser, so the syntax trees are the ones it reads.

## License

Copyright (c) 2026 Kenneth Allen Flegal

Licensed under either of
- Apache License, Version 2.0 ([LICENSE-APACHE][LICENSE-APACHE] or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT][LICENSE-MIT] or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

[mysql]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-mysql/README.md
[LICENSE-APACHE]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-sql/LICENSE-APACHE
[LICENSE-MIT]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-sql/LICENSE-MIT
