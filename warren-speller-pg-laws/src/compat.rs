// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What differs between the PostgreSQL versions the laws run on, each behind one function.

use pgrx::pg_sys;
use std::ffi::c_char;

/// The statement `stmt`, whose text is `text`, analyzed and rewritten, with no parameters.
pub(crate) unsafe fn analyze_and_rewrite(
    stmt: *mut pg_sys::RawStmt,
    text: *const c_char,
) -> *mut pg_sys::List {
    #[cfg(feature = "pg14")]
    {
        pg_sys::pg_analyze_and_rewrite(stmt, text, std::ptr::null_mut(), 0, std::ptr::null_mut())
    }
    #[cfg(not(feature = "pg14"))]
    {
        pg_sys::pg_analyze_and_rewrite_fixedparams(
            stmt,
            text,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        )
    }
}
