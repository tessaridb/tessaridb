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
    Aggregate, Answer, Approximation, ArithmeticOp, Assignment, ColumnDeclaration, ConsumerSource,
    CreateTarget, DeleteBound, Direction, EdgeClause, EdgeEndpoints, EdgeOrdering, Edit, Expr,
    ExprKind, Field, FieldMapping, FieldPath, Hop, Identity, InfoSubject, JoinSide, Name,
    OnFailure, Ordering, Password, Projected, Projection, RangeExpr, ReachRef, RecordTarget,
    Retention, Script, Select, Source, Statement, StatementKind, TableChange, TableRef, Timeout,
    UserChange, UserGrant, Using, Version, Written,
};
pub use bind::Parameters;
pub use error::{Error, Result};
pub use function::{Function, Purity};
pub use lexer::tokenize;
pub use parser::{parse, parse_expression, parse_read};
pub use render::render;
pub use tessari_types::BinaryOp;
pub use token::{Keyword, Punct, Span, Spanned, Token};

/// The widest vector a declaration may name.
///
/// Two orders of magnitude above anything in use. It exists so that a catalog
/// can hold a declared width as the ordinary small integer every other number in
/// a definition is, and it is stated here rather than inside the parser because
/// the store that writes the declaration back needs the same number: a ceiling
/// each half of the code decided for itself is two ceilings waiting to differ.
///
/// A width above it is refused where the author wrote the number
/// ([`Error::VectorWidthAboveTheCeiling`]), which is the only place the refusal
/// can point at what is wrong.
pub const WIDEST_VECTOR: usize = 65_536;
