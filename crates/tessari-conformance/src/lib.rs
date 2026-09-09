//! The executable definition of TessariQL.
//!
//! `docs/tessariql.md` says what the language is; this crate is where that claim is
//! proved. It exists as a crate of its own, separate from the engine's unit
//! tests, so that "does the language still do what it says" is one command with
//! one answer — and so that the corpora are data a person can read rather than
//! assertions buried in Rust.
//!
//! Three things are checked, and each catches a different failure:
//!
//! - **The corpora** run real scripts against a real store and compare what came
//!   back. This catches the language doing the wrong thing.
//! - **The coverage ratchet** ([`coverage`]) fails when a statement form has no
//!   case, and again when a *function* has none. This catches the language
//!   growing something nobody proved — which it had, four times, before the
//!   second half of that ratchet existed.
//! - **The document extractor** ([`document`]) parses every fenced example in
//!   `docs/tessariql.md` itself. This catches the document and the parser drifting
//!   apart, which is silent in both directions, and it is what the documentation
//!   ratchet in `tests/documented.rs` reads to ask the other question: whether
//!   the document names every kind, function and form the engine has.

#![forbid(unsafe_code)]

pub mod case;
pub mod coverage;
pub mod document;
pub mod runner;

pub use case::{Case, Corpus, Expectation, MalformedCorpus, read};
pub use coverage::{FORMS, form_name, forms_in, function_spellings, uncalled_functions, uncovered};
pub use document::{examples, fenced_blocks, specification, specification_path, split_statements};
pub use runner::{CaseResult, run};
