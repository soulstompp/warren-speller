# Changelog

## 0.1.0

The first release under this name, in the warren-speller repository beside the PostgreSQL speller:
the code of warren-mysql-speller 0.1.1, whose changes up to then are in [its changelog][before],
with these changes:

- A statement holding `||` or `&&` is written back as read. MySQL reads `||` as OR unless the
  session's `sql_mode` has `PIPES_AS_CONCAT`; a condition joined by it was read as concatenation,
  and the respelled statement could return other rows.
- A `UNION ALL` whose arms join the common relation to values of different types or collations is
  written back as read. The union holds those values as one type and compares them as that type,
  and could lose rows.
- A grouped `SELECT` that groups by a number is written back as read. In a side's `GROUP BY` the
  number named a place in that side's select list, and MySQL refused the respelled statement.

[before]: https://docs.rs/crate/warren-mysql-speller/0.1.1/source/CHANGELOG.md
