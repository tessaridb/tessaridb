//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

mod assertion;
mod consumers;
mod creates;
mod geometry_literal;
mod identity;
mod identity_spelling;
mod inserts;
mod lexer;
mod parameters;
mod parser;
mod render;
