<!-- Copyright (c) 2026 Kenneth Allen Flegal -->
<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Changelog

## 0.1.0

The first release: the part of warren-speller-mysql that reads a statement on sqlparser's syntax
tree (a `SELECT`'s relations, joins and conditions, and the statement's text), as a crate of its own
that assumes no dialect. How a column compares is the caller's own type, and the caller gives the
dialect a text is read in and the quote a name is written in.
