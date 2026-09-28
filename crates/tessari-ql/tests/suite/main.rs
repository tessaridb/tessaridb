//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

mod assertion;
mod consumers;
mod creates;
mod fusion;
mod geometry_literal;
mod identity;
mod identity_spelling;
mod inserts;
mod lexer;
mod parameters;
mod parser;
mod render;
mod sharding;
