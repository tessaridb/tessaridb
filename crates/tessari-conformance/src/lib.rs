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
//!   case. This catches the language growing a statement nobody proved.
//! - **The document extractor** (in `tests/`) parses every fenced example in
//!   `docs/tessariql.md` itself. This catches the document and the parser drifting
//!   apart, which is silent in both directions.

#![forbid(unsafe_code)]

pub mod case;
pub mod coverage;
pub mod runner;

pub use case::{Case, Corpus, Expectation, MalformedCorpus, read};
pub use coverage::{FORMS, form_name, forms_in, uncovered};
pub use runner::{CaseResult, run};
