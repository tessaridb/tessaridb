//! What a script says, once the words are gone.
//!
//! Two properties of this tree are deliberate.
//!
//! **Every node carries a span.** Not for error messages alone — execution
//! raises failures too, and "no index on `email`" is a different message when it
//! can point at the `email` the author wrote.
//!
//! **Nothing here is resolved.** A table is a name, not a [`TableId`]; a name
//! becomes an id only inside a transaction, by reading the catalog, because that
//! is the only place the answer is true. A tree that carried ids would have to
//! be re-parsed whenever a definition changed under it.
//!
//! [`TableId`]: bgv_db_types::TableId

use bgv_db_types::{RecordId, Value};

use crate::token::Span;

/// A parsed script: statements in the order they were written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The statements, in source order.
    pub statements: Vec<Statement>,
    /// The whole source.
    pub span: Span,
}

/// One statement, and where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// What the statement does.
    pub kind: StatementKind,
    /// Where it sits in the source.
    pub span: Span,
}

/// The statement forms this milestone accepts.
///
/// Flat rather than grouped by family: execution matches on it once, and a
/// grouping would only move the match one level down.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatementKind {
    /// `USE NAMESPACE prod DATABASE orders` — at least one of the two.
    Use {
        /// The namespace to work in, when the statement names one.
        namespace: Option<Name>,
        /// The database to work in, when the statement names one.
        database: Option<Name>,
    },
    /// `DEFINE NAMESPACE prod`
    DefineNamespace {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE DATABASE orders`
    DefineDatabase {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE TABLE users`
    DefineTable {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE SPACE sessions` — a table whose records hold a single value.
    DefineSpace {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE INDEX by_email ON users FIELDS email UNIQUE`
    DefineIndex {
        /// The index's name, unique within its table.
        name: Name,
        /// The table it indexes.
        table: TableRef,
        /// The fields it projects, in order.
        fields: Vec<Name>,
        /// Whether two records may share one entry.
        unique: bool,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP TABLE users` — removes the definition, not the records.
    DropTable {
        /// The table to undefine.
        table: TableRef,
    },
    /// `DROP INDEX by_email ON users`
    DropIndex {
        /// The index's name.
        name: Name,
        /// The table it indexes.
        table: TableRef,
    },
    /// `CREATE users:1 = { … }`
    Create {
        /// The record to write.
        target: RecordTarget,
        /// Its whole content.
        value: Expr,
    },
    /// `SELECT * FROM …`
    Select(Select),
    /// `UPDATE users:1 = { … }` — the value is replaced, never merged.
    Update {
        /// The record to write.
        target: RecordTarget,
        /// Its whole new content.
        value: Expr,
    },
    /// `DELETE users:1`
    Delete {
        /// The record to remove.
        target: RecordTarget,
    },
    /// `GET sessions:'abc'` as a statement of its own.
    Get {
        /// The key to read.
        target: RecordTarget,
    },
    /// `SET sessions:'abc' = …`
    Set {
        /// The key to write.
        target: RecordTarget,
        /// The whole value.
        value: Expr,
    },
    /// `DEL sessions:'abc'`
    Del {
        /// The key to remove.
        target: RecordTarget,
    },
    /// `KEYS FROM sessions RANGE 'a'..'m'`
    Keys {
        /// The space to list.
        space: TableRef,
        /// The range of keys, when the statement bounds it.
        range: Option<RangeExpr>,
    },
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `CANCEL`
    Cancel,
}

/// A read of records.
///
/// The projection is always every field at this milestone, so it is not carried:
/// a field carrying one possible value says less than its absence does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Which access path the statement resolves to.
    pub from: Source,
    /// Where the statement sits in the source.
    pub span: Span,
}

/// The three access paths the store has, named by what the statement targets.
///
/// Which one runs is decided here, by the shape of the statement, and not by a
/// cost model — there is no planner at this milestone and nothing pretends
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Source {
    /// One record, by its identity.
    Record(RecordTarget),
    /// Every record of a table.
    Table(TableRef),
    /// The records an index points at.
    Index {
        /// The table being read.
        table: TableRef,
        /// The field the filter names; it must carry an index.
        field: Name,
        /// The value the field must hold.
        value: Box<Expr>,
    },
}

/// A value written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// Where it sits in the source.
    pub span: Span,
}

/// The expression forms this milestone accepts.
///
/// There is no arithmetic, no comparison and no call: an expression is a value,
/// a container of expressions, or a read. The reads are what make a key-value
/// value usable inside a record statement without either model knowing about
/// the other.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExprKind {
    /// A literal that is already a whole value.
    Literal(Value),
    /// A table named in a value position.
    Table(TableRef),
    /// A record named in a value position: `users:1`.
    Record(RecordTarget),
    /// `[a, b]`
    Array(Vec<Expr>),
    /// `set [a, b]`
    Set(Vec<Expr>),
    /// `{ name: 'ada' }`
    Object(Vec<Field>),
    /// `1..10`, `1..=10`
    Range(RangeExpr),
    /// `GET sessions:'abc'` in a value position.
    Get(RecordTarget),
    /// `(SELECT * FROM users:1)` in a value position.
    Select(Box<Select>),
}

/// One field of an object literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The field's name.
    pub name: Name,
    /// Its value.
    pub value: Expr,
}

/// A span between two values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeExpr {
    /// The lower bound, always included.
    pub start: Box<Expr>,
    /// The upper bound.
    pub end: Box<Expr>,
    /// Whether the upper bound is included: `..=` rather than `..`.
    pub inclusive: bool,
}

/// A name as written, with where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    /// The name itself. Case-sensitive, unlike a keyword.
    pub text: String,
    /// Where it sits in the source.
    pub span: Span,
}

/// A table, optionally qualified by its database.
///
/// An explicit qualification always wins over the session's context, so that a
/// script that says which database it means cannot be run against another one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// The database, when the name is qualified: `orders.users`.
    pub database: Option<Name>,
    /// The table's own name.
    pub name: Name,
    /// Where the whole reference sits.
    pub span: Span,
}

/// One record, by table and identity: `users:1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordTarget {
    /// The table the record lives in.
    pub table: TableRef,
    /// Its identity within that table.
    pub id: RecordId,
    /// Where the whole reference sits.
    pub span: Span,
}
