//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

mod console;
mod console_tokens;
mod crossing;
mod objects;
mod routes;
mod sessions;
mod vault_responses;
mod watch;
