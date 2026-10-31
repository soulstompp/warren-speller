// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! warren_speller_pg_laws: a PostgreSQL extension that installs warren-speller-pg's planner hook,
//! and holds the speller's laws. pgrx runs a crate's `#[pg_test]`s only in the extension it builds,
//! and the speller builds none, so its laws run here: one module for each of its three identities,
//! one for the question that reaches all three, one for what a respelled statement reads, as whom,
//! what its plan rests on and how it is planned when its spelling fails, and one that makes the
//! tables they run on, with unique keys alone or with the B-trees a DBA would add. It is never
//! published.

use pgrx::prelude::*;

#[cfg(any(test, feature = "pg_test"))]
mod catalogue;
#[cfg(any(test, feature = "pg_test"))]
mod compat;
#[cfg(any(test, feature = "pg_test"))]
mod distribute;
#[cfg(any(test, feature = "pg_test"))]
mod fold;
#[cfg(any(test, feature = "pg_test"))]
mod keep;
#[cfg(any(test, feature = "pg_test"))]
mod purchase_question;
#[cfg(any(test, feature = "pg_test"))]
mod reads;

::pgrx::pg_module_magic!(name, version);

/// Installs warren-speller-pg's planner hook. A statement is respelled only once the library is
/// loaded.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    warren_speller_pg::init();
}

/// Laws that need a session of their own, outside a test's transaction.
#[cfg(test)]
mod sessions {
    /// A session on the test server, started with the extension installed.
    fn session() -> postgres::Client {
        pgrx_tests::run_test(
            "nothing_is_respelled_in_a_parallel_operation",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .expect("the test server did not start");
        pgrx_tests::client()
            .expect("no session on the test server")
            .0
    }

    #[test]
    fn a_session_that_loads_the_library_respells_its_next_statement() {
        let mut setup = session();
        setup
            .batch_execute(
                "DROP TABLE IF EXISTS late_buys, late_holdings, late_builders, late_lines; \
                 CREATE TABLE late_builders (builder_id int PRIMARY KEY, zone int NOT NULL); \
                 INSERT INTO late_builders SELECT g, g % 7 FROM generate_series(1, 200) g; \
                 CREATE TABLE late_holdings (builder_id int NOT NULL, row_no int NOT NULL, \
                                             set_num text NOT NULL, PRIMARY KEY (builder_id, row_no)); \
                 INSERT INTO late_holdings SELECT w.builder_id, r, 's' || ((w.builder_id * 17 + r * 31) % 50) \
                 FROM late_builders w CROSS JOIN generate_series(1, 5) r; \
                 CREATE TABLE late_buys (id int PRIMARY KEY, builder_id int NOT NULL, row_no int NOT NULL); \
                 INSERT INTO late_buys SELECT g, h.builder_id, h.row_no FROM generate_series(1, 2000) g \
                     JOIN late_holdings h ON h.builder_id = 1 + (g * 7919) % 1000 % 200 \
                                         AND h.row_no = 1 + (g * 7919) % 1000 / 200; \
                 CREATE TABLE late_lines (set_num text NOT NULL, part int NOT NULL, qty int NOT NULL); \
                 INSERT INTO late_lines SELECT 's' || (g % 50), g % 13, 1 + g % 4 \
                 FROM generate_series(1, 1000) g; \
                 ANALYZE late_builders; ANALYZE late_holdings; ANALYZE late_buys; ANALYZE late_lines",
            )
            .unwrap();
        // a new session that loads the library first
        let mut fresh = session();
        fresh
            .batch_execute("LOAD 'warren_speller_pg_laws'")
            .unwrap();
        let first: String = fresh
            .query(
                "EXPLAIN (COSTS OFF, VERBOSE) SELECT w.zone, l.part, count(DISTINCT b.id), sum(l.qty) \
                 FROM late_buys b JOIN late_holdings h ON h.builder_id = b.builder_id AND h.row_no = b.row_no \
                     JOIN late_builders w ON w.builder_id = h.builder_id \
                     JOIN late_lines l ON l.set_num = h.set_num GROUP BY 1, 2",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        // the first side's grouping, read by the join and by the second side
        assert_eq!(first.matches("CTE Scan on side_a").count(), 2, "{first}");
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
