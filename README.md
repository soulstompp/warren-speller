# warren-speller

Some queries are slow only because of how they are written. A grouping over a join builds every
pair of rows before it counts anything; group each side first and join the groups, and the answer is
the same while the join pairs groups instead of rows. A speller plans a statement through such a
spelling, wherever the statement and the catalog's unique keys show it returns the same rows, and
leaves every other statement as written.

This repository holds one speller for each engine, each a crate of its own:

- [`warren-speller-pg`](warren-speller-pg/README.md) respells inside PostgreSQL's planner, on the
  statement as PostgreSQL parsed it. An extension installs it from `_PG_init`; the surveyor,
  [warren-surveyor-pg](https://github.com/soulstompp/warren-surveyor), is one.

Each crate's README sets out every rule as what it changes, the read it cancels and when it applies,
so that a database engine can take any of them into its own planner.

## License

Copyright (c) 2026 Kenneth Allen Flegal

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
