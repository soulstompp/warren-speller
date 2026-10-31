<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# warren-speller-mysql

*Read less, return the same rows.*

A MySQL statement often says more than its answer needs: a table joined in every arm of a
`UNION ALL` is read once per arm, and a grouped join pairs every row of one side with every
matching row of the other before it groups them. `warren-speller-mysql` respells such a statement
so that it reads less, and returns exactly the rows the statement as written returns.

It has three rules. Each is an identity of SQL, so each is exact. Each reads only the statement and
the database's catalog, its columns and unique keys: never the rows, and never a statistic, so a
statement is respelled alike whatever other indexes its tables have. Where a rule's conditions
cannot be proved from those two, the rule leaves the statement as it is.

It is a library, licensed `MIT OR Apache-2.0` so that a database, a proxy or a driver can take the
rules as they are.

## A first example

Each zone's collection rows, their row numbers summed, and the least set they hold. A builder's
zone is its city's, reached through its street and postcode:

```sql
SELECT cy.zone, COUNT(*) AS n, SUM(c.row_no) AS s, MIN(c.set_num) AS m
FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id
JOIN lego_streets st ON st.street_id = b.street_id
JOIN lego_postcodes pc ON pc.postcode_id = st.postcode_id
JOIN lego_cities cy ON cy.city_id = pc.city_id
GROUP BY cy.zone
```

As written, MySQL joins every builder to every one of their collection rows and groups the pairs by
zone. Respelled, each side is grouped by the column the join meets it on first, and the collection
rows are grouped only for the builders the other side holds:

```sql
WITH side_a (k1, g1, n) AS (
  SELECT b.builder_id, cy.zone, COUNT(*) FROM lego_builders b
  JOIN lego_streets st ON st.street_id = b.street_id
  JOIN lego_postcodes pc ON pc.postcode_id = st.postcode_id
  JOIN lego_cities cy ON cy.city_id = pc.city_id
  GROUP BY b.builder_id, cy.zone)
SELECT side_a.g1 AS zone, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS n,
       SUM(side_b.p2 * side_a.n) AS s, MIN(side_b.p3) AS m
FROM side_a
JOIN (SELECT c.builder_id, COUNT(*), SUM(c.row_no), MIN(c.set_num) FROM lego_collection c
      WHERE c.builder_id IN (SELECT k1 FROM side_a) GROUP BY c.builder_id)
  AS side_b (k1, n, p2, p3)
ON side_a.k1 = side_b.k1
GROUP BY side_a.g1
```

The join now meets one row per builder on each side, and the rows it groups by zone are those.
(The respelled statement is written on one line; it is broken here to be read.)

## Using it

```rust
use warren_speller_mysql::{catalog::Catalog, respell};

let catalog = Catalog::parse(&std::fs::read_to_string("catalog.tsv")?)?;
let respelled = respell(&catalog, "SELECT …")?;
```

- **The catalog.** `catalog::query` writes the query whose rows are a database's catalog:

  ```sh
  mysql --batch --skip-column-names -e "$QUERY" > catalog.tsv
  ```

  `Catalog::parse` reads that output. `Catalog::from_rows` reads the same rows from a driver, each
  value as text or NULL. A caller can also build a `Catalog` itself.
- **`respell(catalog, statement)`** returns the statement respelled, followed by a line break; or
  the statement exactly as it was read when no rule applies. A statement it cannot read is
  `Refused`, with the reason.
- **`respell_with(catalog, statement, first)`** makes the caller's own change to the parsed query
  first, then applies the rules, and writes the statement once.
- **`respell_query(catalog, &mut query)`** applies the rules to a query the caller has parsed. The
  crate re-exports [sqlparser](https://crates.io/crates/sqlparser), so the syntax trees are the ones
  it reads.

## Where the rules apply

- **In the statement, every WITH query and every derived table**, from the innermost out. At each
  query, the first rule is tried, then the second with the third.
- **Not in** a recursive WITH query, a `LATERAL` derived table, or a query nested in an expression
  (a subquery in a `WHERE` or a select list).
- **A `?` is never copied or moved,** so the respelled statement binds the same parameters in the
  same order. A `SELECT` holding one is left as written; a `LIMIT ?` after a grouped `SELECT` stays
  where it is.
- **Nothing a rule would repeat or reorder is moved:** a function whose value may change between
  two calls (`RAND`, `UUID`, `UUID_SHORT`, `SYSDATE`, `SLEEP`, `BENCHMARK`, the lock functions,
  `LAST_INSERT_ID`, `RANDOM_BYTES`), a user variable, or a window function.
- **A statement holding `||` or `&&` is written back as read.** MySQL reads `||` as `OR` unless
  the session's `sql_mode` has `PIPES_AS_CONCAT`, which the statement does not show; and the parser
  binds `&&` more tightly than MySQL, which reads it as `AND`.

## Rule 1: a relation every `UNION ALL` arm joins is joined once

A relation joined on the same condition in every arm joins the union of the rest of each arm:

```
(A ⋈ B₁) UNION ALL (A ⋈ B₂)  =  A ⋈ (B₁ UNION ALL B₂)
```

**What it rewrites.**

```sql
SELECT c.builder_id, i.id AS inventory_id, 1 AS copies
FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1
UNION ALL
SELECT c.builder_id, ci.id, sub.quantity
FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1
JOIN lego_inventory_sets sub ON sub.inventory_id = i.id
JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1
```

becomes

```sql
SELECT c.builder_id, arms.c1 AS inventory_id, arms.c2 AS copies
FROM lego_collection c
JOIN (SELECT i.set_num, i.id, 1 FROM lego_inventories i WHERE i.version = 1
      UNION ALL
      SELECT i.set_num, ci.id, sub.quantity FROM lego_inventories i
      JOIN lego_inventory_sets sub ON sub.inventory_id = i.id
      JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1
      WHERE i.version = 1) AS arms (k1, c1, c2)
ON c.set_num = arms.k1
```

The collection is read once, where it was read once per arm.

**When.** Every one of these holds:
- The query is two or more `SELECT`s joined by `UNION ALL` alone, with no `ORDER BY`, `LIMIT`,
  `FETCH` or locking clause around them.
- Each arm is a plain `SELECT`: no `DISTINCT`, `GROUP BY`, `HAVING`, window, optimizer hint or
  `INTO`; a select list of expressions, none of them `*` and none holding an aggregate; every join
  an inner join (`JOIN`, `INNER JOIN` or `CROSS JOIN`, `ON` a condition or on none); nothing the
  rules never move, and no subquery.
- Every relation of every arm is a table the catalog holds, a WITH query in scope, or a derived
  table with an alias; what each returns can be read; and every column the arm names is a column
  of one of them.
- **The common relation:** one table or WITH query of the first arm, read by the same name (its
  alias aside) in every arm, and exactly once in each.
- The conditions are the `WHERE` and every `ON`, split at `AND`. In each arm, every condition that
  reads the common relation and another is an equality of one of its columns with an expression
  that does not read it, and there is at least one. The common relation's columns joined so are the
  same in every arm.
- What each arm joins each of those columns to compares as one known type, the same in every arm:
  whole numbers, decimals, text in one collation, and so on. The union holds those values as one
  type, and the join compares them as that type.
- The conditions on the common relation alone are the same in every arm.
- The arms return as many columns each, and each column returned reads the common relation alone,
  as the same expression in every arm, or does not read it in any arm.
- The first arm names every column it returns: an alias, or a column reference.
- In each arm, the relations other than the common one are joined to one another by the arm's
  conditions.

**What it writes.** The common relation, joined on its columns to a derived table `arms`: the union
of the rest of each arm, returning first what the arm joined the common relation to (`k1`, `k2`,
…), then the returned columns that do not read the common relation (`c1`, `c2`, …). Conditions of
the rest of an arm stay in its arm; conditions on the common relation alone go to the `WHERE`. The
columns the union returns keep the first arm's names. The first relation of the first arm for which
every condition holds is taken.

## Rule 2: a grouped join is grouped on each side first

A grouped `SELECT` over an inner join that splits in two, at columns `k`, groups each side by its
own group keys and `k`, joins the groups at `k`, and groups again, each aggregate taken apart:

| as written | on its side | again, over the joined groups |
|---|---|---|
| `COUNT(*)` | `COUNT(*)` on each side, `n` | `CAST(SUM(a.n * b.n) AS SIGNED)` |
| `COUNT(x)` | `COUNT(x)` | `CAST(SUM(count * other side's n) AS SIGNED)` |
| `COUNT(DISTINCT u)` | `COUNT(*)` | `CAST(SUM(n) AS SIGNED)` |
| `SUM(x)` | `SUM(x)` | `SUM(sum * other side's n)` |
| `MIN(x)`, `MAX(x)` | `MIN(x)`, `MAX(x)` | `MIN`, `MAX` of those |

**What it rewrites.** The first example, above.

**When.** Every one of these holds:
- The query's body is one `SELECT` with a `GROUP BY` of at least one item, no `WITH ROLLUP`, no
  `HAVING`, no `DISTINCT`, no optimizer hint and no `INTO`, and every join an inner join. An
  `ORDER BY` and a `LIMIT` may follow it; `FETCH` and locking may not.
- Nothing in the `SELECT` or its `ORDER BY` is a `?`, a subquery, or anything the rules never move.
- Every item of the select list is named: an alias, or a column reference.
- A `GROUP BY` item is a position in the select list, a name the select list gives (where no
  relation has a column of that name, as MySQL reads it), or an expression of columns; none holds
  an aggregate, and none is a number, written there or in the select list item it names. An
  `ORDER BY` item is a position, a name the select list gives, or an expression of the `GROUP BY`
  items and aggregates, with no `NULLS FIRST` or `NULLS LAST`.
- It reads between 2 and 16 relations: tables the catalog holds, WITH queries in scope, derived
  tables with an alias, each whose columns can be read.
- Every aggregate is `COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT u, …)` of plain columns, `SUM(x)` of
  an exact expression (whole numbers and decimals, added, subtracted and multiplied), `MIN(x)` or
  `MAX(x)`, with no `FILTER` or window. `AVG`, `GROUP_CONCAT`, `SUM(DISTINCT …)`, a sum of
  floating-point values or of a division, and every other aggregate leave the statement as
  written.
- **A cut holds.** The relations split into two sides, and all of these hold:
  - every condition reading both sides is an equality of a column of one side with a column of the
    other, and there is at least one;
  - every `GROUP BY` item reads one side, and every aggregate reads one side;
  - each side's relations are joined to one another by the side's conditions, counting the
    equalities the join implies (two columns of one side equal to one column of the other);
  - the columns joined across the cut, taken with every column equal to them, compare as one known
    type: whole numbers, decimals, text in one collation, and so on;
  - **a counted side** has a grain: a relation from which its joins reach at most one row of every
    other relation of the side. A relation is reached when equalities fix every column of one of
    its unique keys to columns of relations already reached, of the same type, or to constants of
    that type. Both sides are counted for `COUNT(*)` and `COUNT(x)`, the side it reads for
    `COUNT(DISTINCT u)`, and the other side for a `SUM`;
  - for `COUNT(DISTINCT u, …)`, the columns `u` are of one grain of the side, hold no NULL, and
    include every column of one of its unique keys. The rule counts rows there because the catalog
    says `u` is unique, so a catalog naming a unique key the table does not hold gives a wrong
    count.
- **The unique keys** of a table are the catalog's. A WITH query or a derived table has one when it
  returns every column it groups by, as plain references that hold no NULL; when, with no `GROUP
  BY` and no aggregate, it returns every column of a unique key of a relation from which its joins
  reach at most one row of every other; or when it is `DISTINCT` over columns that hold no NULL.
- **The cut taken** is the one whose smaller side has the fewest relations, the earliest relations
  first among those.
- **When no cut holds,** each WITH query read once and each derived table is read as its own
  relations, and the cuts are tried again, up to four times. Such a WITH query or derived table is a
  plain `SELECT` over inner joins with no aggregate, no `ORDER BY` or `LIMIT` and no WITH of its
  own, whose relations take no name the outer `SELECT` gives another relation, and none of whose
  tables is named like a later WITH query of the statement. A WITH query read so is removed.

**What it writes.** Each side as a grouped `SELECT` of its relations and its conditions, returning
the columns it is joined on (`k1`, …), the `GROUP BY` items it reads (`g1`, … by their place in
the `GROUP BY`), its row count
(`n`) when it is counted, and its part of each aggregate (`p1`, …). The two are joined on `k` and
grouped by the `GROUP BY` items, the aggregates taken again as in the table. The `ORDER BY` and
`LIMIT` stay outside, over the respelled items.

## Rule 3: each side keeps only the keys the other holds

In an inner join, a row of one side whose `k` the other side does not hold meets nothing:

```
B ⋈ A  =  (B ⋉ π_k A) ⋈ A
```

**What it rewrites.** Wherever rule 2 applies, one side is held: the side a `COUNT(DISTINCT u)`
reads, else the one side whose rows are counted, else the first. The held side becomes a WITH
query, and the other side's `WHERE` gains `k IN (SELECT k1 FROM held)`, or `(k, …) IN (SELECT k1,
… FROM held)` for more columns. The other side is grouped only over the values of `k` the held
side holds.

**When.** Exactly when rule 2 applies.

## The catalog

For each table of one database:
- each column, in the table's order: its name; how it compares (whole numbers, decimals,
  floating-point values, text with its collation, `DATETIME`, `DATE`, `TIMESTAMP`, or another type
  by name); whether it may hold NULL; whether `SELECT *` lists it (an invisible column it does
  not); the expression a generated column is computed from; whether an index holds it as a key
  part;
- its unique keys: the primary key, and every unique index whose parts are all columns that hold no
  NULL, visible or not.

The rules read the types, the NULLs, the listed columns and the unique keys. They read no other
index, so a statement is respelled alike whatever keys its tables have.

`catalog::query` writes one `UNION ALL` of `information_schema.COLUMNS` and
`information_schema.STATISTICS` for the database: nine values a row, a row for each column and a
row for each part of each index. Rows of two databases, or of another shape, are refused.

## Reading and writing a statement

- **The parser** is sqlparser's MySQL dialect, with string literals kept exactly as written.
  [warren-speller-sql][sql] reads the statement's relations, joins and conditions on its tree.
- **One statement.** Two, or one that cannot be read, is refused with the reason. Anything but a
  query is written back as read.
- **What it writes.** A statement no rule applies to is written back exactly as it was read. A
  respelled statement is written on one line, in the parser's spelling, ending in `;` when the
  statement did.
- **Names it adds.** `side_a`, `side_b` and `arms` are taken only when the statement uses no such
  name; otherwise a number follows them (`side_a2`). The columns they return are named `k1`, `g1`,
  `n`, `p1`, `c1` and so on.
- **Table names.** A table named without a database is a WITH query of that name in scope, or else
  the catalog's, as in a session whose database is the catalog's; one named with the catalog's
  database is the catalog's too. A table of another database is not read, and no rule applies to a
  query that reads one.

## Taking a rule into a database

Each rule reads a parsed `SELECT` and the catalog and writes a `SELECT`. A server, a proxy or a
driver can apply it after parsing and before planning, and plan the result like any other
statement: no hint, optimizer switch or forced index is added. The conditions above are the whole
of what each rule checks, and the tests (`tests/respell.rs`) give, for each condition, a statement
it leaves as written.

## Build and test

```sh
cargo build
cargo test
```

The tests read `tests/catalogs/lego.tsv`: the catalog of the tables
[sqlc-brickgen](https://crates.io/crates/sqlc-brickgen) makes, as MySQL holds them with their keys
and indexes, in the rows `catalog::query` returns. They need no server.

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

[LICENSE-APACHE]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-mysql/LICENSE-APACHE
[LICENSE-MIT]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-mysql/LICENSE-MIT
[sql]: https://github.com/soulstompp/warren-speller/blob/HEAD/warren-speller-sql/README.md
