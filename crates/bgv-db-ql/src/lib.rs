//! bgvQL for `bgv-db`.
//!
//! bgvQL is the only way structure comes into existence in this store: there is
//! no schema file, no migration DSL and no side door in the API. That makes this
//! crate the front door, and `docs/bgvql.md` its normative specification — the
//! milestone-1 subset, bounded by one rule: it is exactly what the store
//! underneath can execute today.
//!
//! This crate holds the lexer today. The parser and the abstract syntax follow,
//! against the same specification.

#![forbid(unsafe_code)]

mod error;
mod lexer;
mod token;

pub use error::{Error, Result};
pub use lexer::tokenize;
pub use token::{Keyword, Punct, Span, Spanned, Token};
