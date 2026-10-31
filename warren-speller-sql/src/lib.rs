// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a SQL statement reads, read from [sqlparser]'s syntax tree: each SELECT's relations, with
//! the columns each returns and its unique keys; its joins; and its conditions, each by the
//! relations it reads ([`relations`]). And a statement's text read in any dialect sqlparser reads,
//! its string literals kept as written, and a name written in the dialect's quotes ([`text`]).
//!
//! It changes nothing: a speller reads a statement through it and decides what to write. What a
//! database's catalog holds and what its dialect's functions do are the speller's. The crate
//! re-exports sqlparser, so that a caller's syntax trees are the ones it reads.

pub mod relations;
pub mod text;

pub use sqlparser;
