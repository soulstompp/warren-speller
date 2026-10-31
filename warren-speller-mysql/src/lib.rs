// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MySQL statements respelled to read less, returning exactly the rows the statement as written
//! returns. Three rules, each read against a database's [catalog](catalog::Catalog):
//!
//! - **A relation every UNION ALL arm joins** is joined once, to the union of the rest of each arm.
//! - **A grouped SELECT over a join that splits in two** is grouped on each side before the join.
//! - **Each side keeps only the keys the other holds**: the side not holding the rows a count
//!   counts is grouped only over the joined values the other side holds.
//!
//! A statement none of them applies to is written back exactly as it was read, and a `?` parameter
//! is never copied or moved. Statements are read with [sqlparser]'s MySQL dialect, which this crate
//! re-exports so that a caller's syntax trees are the ones it reads.

pub mod catalog;
mod respell;
pub mod sql;

pub use respell::{respell, respell_query, respell_with};
pub use sqlparser;

use std::fmt;

/// Why a statement or a catalog was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}
