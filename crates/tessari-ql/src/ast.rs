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
//! The enums here are deliberately **not** `#[non_exhaustive]`. They are the
//! contract between the parser and whatever executes the tree, both of which
//! version together in this workspace, and exhaustive matching is what makes
//! adding a statement to the grammar fail to compile until something runs it.
//! Sealing them would buy version tolerance nobody needs and pay for it with a
//! wildcard arm that silently accepts every future statement.
//!
//! [`TableId`]: tessari_types::TableId

mod expr;
mod info;
mod select;
mod statement;
mod tables;
mod users;
use tessari_types::{Assertion, FieldKind, Path, RecordId};

use crate::token::Span;
pub use expr::{
    Aggregate, ArithmeticOp, Expr, ExprKind, Field, GroupClauses, Identity, RangeExpr, Retention,
    SetCondition, SpaceBound, TopicClauses, Written,
};
pub use select::{
    Admitted, AnsweredBy, Approximation, Fill, FillMode, Fusion, Hop, JoinSide, Ordering, PathTo,
    Projected, Projection, Select, Source, Staleness, Timeout, Using, Version,
};
pub use statement::{
    BackupForm, RollupCompute, SearchAsk, SearchCosts, SearchField, SearchMember, SearchOperator,
    StatementKind,
};

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
    /// The acknowledgement this write — or this `COMMIT` — asked for itself,
    /// over its namespace's (ADR-0106 D2). `None` on everything else, and on a
    /// write that said nothing.
    pub acknowledge: Option<tessari_types::Acknowledge>,
    /// Whether this write — or this `COMMIT` — said `ACROSS LEADERS`: it may
    /// commit across ranges led by different nodes, atomically, rather than be
    /// refused for spanning them (ADR-0112 D1). `false` on everything else.
    pub across: bool,
}

/// Where a consumer's messages come from.
///
/// The **group is declared and never derived**. It is a broker-side identity,
/// and deriving it from the node id would be a bug that appears only in a
/// cluster: every node would form its own group, and every node would then
/// consume every message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerSource {
    /// The brokers to reach, as written.
    pub brokers: Vec<String>,
    /// The topic to read.
    pub topic: String,
}

/// One message field, and what it is called in the record.
///
/// Read from a path so a nested payload works, written to a plain name so the
/// record stays flat. That asymmetry is the boundary that keeps this a mapping
/// rather than a transformation language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMapping {
    /// Where to read it in the message.
    pub from: FieldPath,
    /// What it is called in the record.
    pub to: Name,
}

/// What a consumer does with a message it cannot apply.
///
/// Two values, and the absence of a third is the decision. A skip-N mode would
/// let a silent default decide about data loss, and the system that offers one
/// miscounts what it skips: given a message holding several rows it discards
/// *the row*, not the message, so the counter does not count what its name says.
/// A misnamed safety knob is a safety knob set wrong.
///
/// There is no default value either, because a default here is a decision about
/// data loss taken by whoever did not type the clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Halt the consumer and record why.
    Stop,
    /// Bounded retries, then park the payload where the language can find it.
    Quarantine,
}

impl OnFailure {
    /// The word a statement writes it as.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Quarantine => "quarantine",
        }
    }
}

/// What a write answers with.
///
/// Absent by default, because a write's answer is its effect and a store that
/// shipped every changed record back by default would make the common case pay
/// for the rare one. What this removes is the *second statement*: reading back
/// what was just written cost a round trip to learn a value the store had in
/// hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Answer {
    /// No clause: the write reports that it happened and nothing more.
    #[default]
    Nothing,
    /// `RETURN BEFORE` — the record as it stood before the write.
    Before,
    /// `RETURN AFTER` — the record as it stands after it.
    After,
}

/// How much a conditional delete may remove.
///
/// Every `DELETE FROM … WHERE …` carries one, and there is no third variant for
/// "unstated". A predicate wrong by one character is the ordinary way a table is
/// emptied by accident, and the cheapest thing standing between that typo and
/// the store is a clause the author had to write.
///
/// The bound is on what is **removed**, never on what is examined. A bound
/// applied to candidates would make the same statement remove different records
/// on two runs, depending on which index answered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBound {
    /// `LIMIT 100` — stop after this many records have been removed.
    AtMost(u64),
    /// `LIMIT ALL` — every record the condition holds for, however many that is.
    All,
}

/// How an `UPDATE` changes the record it names.
///
/// One statement with two shapes rather than two statements, because both touch
/// exactly **one** record. `DELETE` and `DELETE FROM … WHERE` are two statements
/// for the opposite reason: they differ in how many records they reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// `UPDATE users:1 = { … }` — the record becomes this.
    Whole(Expr),
    /// `UPDATE users:1 SET name = 'grace', visits = visits + 1` — these routes
    /// change and nothing else does.
    Fields(Vec<Assignment>),
    /// `UPDATE users:1 MERGE { address: { city: 'Paris' } }` — the object is
    /// folded into the record, and what it does not name is left alone.
    ///
    /// Distinct from `Fields` rather than sugar for it: `SET` names routes one
    /// at a time and computes each from the record, while this takes a whole
    /// object whose shape is the shape of the change. It is what an HTTP `PATCH`
    /// handler holds, and without it every client builds the same fold by hand.
    ///
    /// Merging is **deep on objects and total on everything else**: where both
    /// sides hold an object the two are merged, and otherwise the incoming value
    /// wins. An explicit `NULL` therefore sets the field to `NULL` — removing a
    /// field is `SET route = NONE`, which says removal out loud.
    ///
    /// The object stands in the **value** position, as every object literal in
    /// this language does, so a bare name inside it is a table and not a route
    /// into the record being changed. That is the one place `MERGE` and `SET`
    /// read differently, and it is deliberate: `{ a: b }` cannot mean two things
    /// depending on which verb precedes it. `MERGE { visits: visits + 1 }` is
    /// therefore not the way to say that — `SET visits = visits + 1` is, and
    /// computing from the record is what `SET` is for. What `MERGE` is for is a
    /// whole object arriving from outside, which is usually `MERGE $patch`.
    Merge(Expr),
}

/// One route of a record, and what it becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// The route into the record.
    pub route: FieldPath,
    /// What it becomes, read against the record **as it was** — so
    /// `SET a = b, b = a` swaps rather than assigning `b` to both.
    pub value: Expr,
}

pub use info::InfoSubject;
pub use tables::{
    AutoSplit, ColumnDeclaration, Direction, EdgeClause, EdgeEndpoints, EdgeOrdering, TableChange,
    TableExpiry, WriteExpiry,
};
pub use tessari_types::BinaryOp;
pub use users::{Credential, Password, UserChange, UserGrant};

/// A route to a value inside a record, as written.
///
/// Separate from [`Name`] rather than replacing it, because the two are asked
/// for in different positions and only one of them may be nested: a table is
/// named, an index is named, a field *declaration* names a top-level field, and
/// only a filter and an index's projection address a value that may sit deeper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    /// The route itself.
    pub path: Path,
    /// Where it sits in the source.
    pub span: Span,
}

/// A name as written, with where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    /// The name itself. Case-sensitive, unlike a keyword.
    pub text: String,
    /// Where it sits in the source.
    pub span: Span,
}

/// The one thing an [`AlterNamespace`](StatementKind::AlterNamespace) changes —
/// one per statement, for [`UserChange`]'s reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceChange {
    /// `REPLICATION FACTOR 3` or `REPLICATION NONE`.
    Replication(tessari_types::Replication),
    /// `ACKNOWLEDGE MAJORITY [OR WEAKER]` (ADR-0106 D2).
    Acknowledge(tessari_types::Acknowledgement),
}

/// The one thing an [`AlterReplica`](StatementKind::AlterReplica) changes about
/// a peer's row — one per statement, for [`UserChange`]'s reason (Q-892).
///
/// The row is amended in place, so what it does not name — the node it is
/// bound to above all — stays as it was: dropping a bound row and declaring it
/// again tombstones that node (ADR-0108 D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaChange {
    /// `LEADS SHARD prod.shop.orders 2 [PREFERRED]`, or `LEADS NONE`
    /// (ADR-0098; `PREFERRED`, G053 SG5b).
    Leads {
        /// The range placed, or `None` to give the placement up.
        range: Option<ReachRef>,
        /// Whether this candidate is the one the range's leader yields to.
        preferred: bool,
    },
    /// `AT 'b2:9001'` — where its peer door answers now.
    At(String),
    /// `ROLES serving, writable` — what it is for, as the words written.
    Roles(Vec<Name>),
    /// `CLIENTS AT 'b2:9080'`, or `CLIENTS NONE` (ADR-0101).
    ClientsAt(Option<String>),
    /// `HTTP AT 'http://b2:8000'`, or `HTTP NONE`.
    HttpAt(Option<String>),
    /// `REGION 'eu'`, or `REGION NONE` (G057 C3).
    Region(Option<String>),
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

/// How far an authority goes, as the statement wrote it.
///
/// Every spelling is led by a keyword — `STORE`, `NAMESPACE`, `DATABASE` — so
/// that no table name can be read as a reach. The exception is the bare
/// `<namespace>.<database>` that `DEFINE USER … ON prod.orders` has always
/// accepted, which is kept meaning what it has always meant.
///
/// Ids are absent here because a reach is written with names and stored with
/// ids, and the resolution needs a transaction this tree does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReachRef {
    /// `STORE` — every namespace, and the store-level surface above them.
    Store,
    /// `NAMESPACE prod` — one namespace and every database in it.
    Namespace(Name),
    /// `DATABASE prod.orders`, or the bare `prod.orders` — one database.
    Database(TableRef),
    /// `SHARD prod.shop.orders 2` — one shard of one split table (G031).
    ///
    /// Only a subscription says it: `REPLICATES` reads it and no grant does,
    /// because no authority comes at a shard's reach.
    Shard {
        /// The namespace.
        namespace: Name,
        /// The database.
        database: Name,
        /// The split table.
        table: Name,
        /// Which of its shards, numbered as `INFO FOR TABLE` reports them.
        shard: u32,
    },
}

/// One record, by table and identity: `users:1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordTarget {
    /// The table the record lives in.
    pub table: TableRef,
    /// Its identity within that table.
    pub id: Identity,
    /// Where the whole reference sits.
    pub span: Span,
}

/// Who names the record a `CREATE` writes.
///
/// The verb carries the meaning and the identity's **absence** is the whole
/// signal: `CREATE users = { … }` says the caller has a record and no name for
/// it, and `CREATE users:1 = { … }` says they have both. Nothing else in the
/// statement changes, which is why this is a target rather than a second verb.
///
/// # Why this is not a third [`Identity`] variant
///
/// `Identity` stands in `UPDATE`, `UPSERT`, `DELETE`, `GET`, `PUT`, `RELATE`
/// and every graph reference, and in every one of them the caller is pointing
/// at a record that already exists. *Generated* has no reading there. A variant
/// added to `Identity` would be representable in seven statements to serve one,
/// and [`Identity::fixed`] would have to invent an error for a case its own
/// grammar can never produce. Keeping the choice here means the type says which
/// statements can be written without a name — and the compiler enforces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateTarget {
    /// `CREATE users:1 = { … }` — the caller names the record.
    Named(RecordTarget),
    /// `CREATE users = { … }` — the store names it, under the scheme the table
    /// was declared with.
    Generated(TableRef),
}

impl CreateTarget {
    /// The table the record is written to, either way.
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        match self {
            Self::Named(target) => &target.table,
            Self::Generated(table) => table,
        }
    }
}

#[cfg(test)]
mod tests;
