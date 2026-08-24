//! TessariQL for TessariDB.
//!
//! TessariQL is the only way structure comes into existence in this store: there is
//! no schema file, no migration DSL and no side door in the API. That makes this
//! crate the front door, and `docs/tessariql.md` its normative specification — the
//! milestone-1 subset, bounded by one rule: it is exactly what the store
//! underneath can execute today.
//!
//! This crate reads a script: the lexer turns text into tokens, the parser
//! turns tokens into the abstract syntax. Nothing here executes anything, and
//! nothing here is resolved — a table is a name until a transaction reads the
//! catalog and learns its id.

#![forbid(unsafe_code)]

mod ast;
mod bind;
mod error;
mod function;
mod lexer;
mod parser;
mod render;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod token;

pub use ast::{
    Aggregate, ArithmeticOp, Assignment, Direction, Edit, Expr, ExprKind, Field, FieldPath, Hop,
    Identity, InfoSubject, Name, Ordering, Projected, Projection, RangeExpr, RecordTarget, Script,
    Select, Source, Statement, StatementKind, TableRef, Written,
};
pub use bind::Parameters;
pub use error::{Error, Result};
pub use function::Function;
pub use lexer::tokenize;
pub use parser::{parse, parse_expression};
pub use render::render;
pub use tessari_types::BinaryOp;
pub use token::{Keyword, Punct, Span, Spanned, Token};
