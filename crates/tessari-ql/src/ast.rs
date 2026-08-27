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

use tessari_types::{Assertion, FieldKind, Filter, Path, RecordId, Value};

use crate::function::Function;
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
    /// `DEFINE TABLE users SCHEMAFULL` / `DEFINE TABLE follows EDGE`
    DefineTable {
        /// The name to create.
        name: Name,
        /// Whether the table refuses a field it does not declare.
        schemafull: bool,
        /// Whether the table holds edges, with an index on each endpoint.
        edge: bool,
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
    /// `DEFINE BUCKET media` — a table whose records are files.
    ///
    /// The bytes live in a companion table nothing can name, and the records
    /// here are metadata the store fills in (ADR-0011). Declared with its own
    /// word rather than a flag on `DEFINE TABLE`, because what a caller may do
    /// to it differs: a bucket is written through `PUT` and never by hand.
    DefineBucket {
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
        /// The values it projects, in order. A path indexes a nested value.
        fields: Vec<FieldPath>,
        /// Whether two records may share one entry.
        unique: bool,
        /// Whether the index holds terms rather than whole values.
        search: bool,
        /// Whether the index holds the cells covering each record's geometry.
        spatial: bool,
        /// The distance a vector index's graph is built with, when it is one.
        ///
        /// Carried as the word the author wrote rather than as a parsed kind,
        /// because which distances exist is the store's question and not the
        /// grammar's: a name the store does not know is refused where the store
        /// knows what it knows, with the span the author can see.
        vector: Option<Name>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE FIELD email ON users TYPE string`
    DefineField {
        /// The field's name, unique within its table.
        name: Name,
        /// The table it is declared on.
        table: TableRef,
        /// What the field is allowed to hold.
        kind: FieldKind,
        /// Whether the field must hold a value: present, and not `null`.
        required: bool,
        /// The analyzer this field's text becomes terms by, when it has one.
        analyzer: Option<Name>,
        /// What a write supplying no value uses instead.
        ///
        /// A **value-position** expression, so it cannot read the record it is
        /// filling in — which would be a rule about evaluation order nobody
        /// would guess.
        default: Option<Written>,
        /// What the value must satisfy, beyond its type.
        ///
        /// Already lowered, because the **store** checks it: an assertion is a
        /// closed constraint rather than an expression, so nothing below the
        /// language has to evaluate TessariQL to enforce a schema.
        assert: Option<Assertion>,
        /// Whether re-declaring an existing name is accepted.
        if_not_exists: bool,
    },
    /// `EXPLAIN SELECT …` — the plan a read would take, without taking it.
    ///
    /// A decision nobody can look at is a decision nobody can debug, and one no
    /// test can assert without timing it.
    Explain(Box<Select>),
    /// `INFO FOR TABLE users` — what the catalog holds about one subject.
    ///
    /// A read whose subject is the schema rather than the records. It reports
    /// only what the caller could have found out anyway: which tables a grant
    /// names, which fields a field grant leaves readable. See [`InfoSubject`].
    Info {
        /// What is being asked about.
        subject: InfoSubject,
    },
    /// `BACKUP` or `BACKUP FROM 42` — the store's log as a backup file.
    ///
    /// The one statement whose scope is the **store** rather than the selected
    /// namespace, which is why it needs an owner rather than a table permission:
    /// there is no table for a grant to name.
    Backup {
        /// The sequence the file starts at; absent means the whole log.
        from: Option<u64>,
    },
    /// `DEFINE ANALYZER simple FILTERS lowercase, ascii`
    DefineAnalyzer {
        /// The name to create.
        name: Name,
        /// The filters it applies, in order.
        filters: Vec<Filter>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE USER ada ON prod.orders ROLE editor PASSWORD '…'`
    ///
    /// The password reaches this tree and goes no further: what is stored is a
    /// hash, so no plaintext reaches the log or any replica.
    DefineUser {
        /// The name signed in with.
        name: Name,
        /// The tenancy the user belongs to, or the store when absent.
        scope: Option<TableRef>,
        /// What the user may do.
        role: Name,
        /// The password, as written. Prints as `<redacted>`.
        password: Password,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `ALTER USER ada SET PASSWORD '…'` · `ALTER USER ada SET ROLE editor`
    ///
    /// Changes **one** thing about a user who already exists, and the tenancy is
    /// not one of them: there is no `SET ON`, because widening somebody's reach
    /// is the one change an administrator of a part could use to reach the
    /// whole. Rotating a password and correcting a role are both things an owner
    /// of a namespace does for their own people; moving a user out of that
    /// namespace is not.
    AlterUser {
        /// The user being changed.
        name: Name,
        /// What about them.
        change: UserChange,
    },
    /// `DEFINE NODE ROLES serving, writable ENDPOINTS 'host:9000'`
    ///
    /// The settings that describe **this machine**, written to the local `META`
    /// keyspace where the log cannot carry them (ADR-0018 §1, ADR-0020 §3). A
    /// replica that inherited `writable` from the node it restored would accept
    /// writes it is supposed to forward, and peers told to reach it at the
    /// original's address would reach the original.
    ///
    /// There is no `DEFINE NODE <other> …`, and its absence is a decision
    /// (ADR-0020 §4): you configure a node **on** it, because a statement that
    /// reached across would be a second mechanism for something the range table
    /// already decides, disagreeing the first time a node was unreachable while
    /// its row said otherwise.
    DefineNode {
        /// What the node is for, or nothing to leave the roles alone.
        ///
        /// Words rather than a parsed set, for the reason a vector distance is
        /// carried as written: which roles exist is the store's question and not
        /// the grammar's, so an unknown one is refused where the store knows
        /// what it knows, with the span the author can see.
        roles: Option<Vec<Name>>,
        /// Where peers reach it, or nothing to leave the endpoints alone.
        endpoints: Option<Vec<String>>,
    },
    /// `DEFINE REPLICA second AT 'host:9001'`
    ///
    /// The opposite half: a peer is a fact every node must learn, so it is a
    /// catalog record and it replicates (ADR-0009). This is the statement whose
    /// effect a backup carries, and the identity above is the one it must not —
    /// which is why neither test is the criterion on its own.
    DefineReplica {
        /// The name the peer is known by.
        name: Name,
        /// Where it answers, as written.
        endpoint: String,
        /// What that peer is for, as the words a statement wrote.
        ///
        /// `None` when the declaration did not say, which reads as no roles: a
        /// peer nobody has said takes writes does not take them.
        roles: Option<Vec<Name>>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE CONSUMER orders_in FROM 'broker:9092' TOPIC 'orders' …`
    ///
    /// Ingestion that is **declared rather than scripted**: one statement says
    /// what to read, how to read it, where it lands, and under which group, and
    /// the node runs it because the catalog says so (ADR-0023).
    ///
    /// # Why one object rather than two
    ///
    /// The obvious alternative splits this in half — a thing that consumes and a
    /// thing that writes — which buys composition at the price of an ordering
    /// nobody controls: the consumer can start before its destination exists,
    /// and the binding between them lives inside a third object where nothing
    /// names it as a relationship. Making the destination a *field* removes that
    /// race by construction, because a field is resolved before the consumer is
    /// started rather than raced against it.
    ///
    /// # What it refuses to say
    ///
    /// There is no exactly-once, and there is no schema inference. Both refusals
    /// are also reported by `INFO FOR CONSUMER`, because a guarantee documented
    /// away from the point of configuration is one that will be misread.
    DefineConsumer {
        /// The consumer's catalog identity.
        name: Name,
        /// Where the messages come from.
        source: ConsumerSource,
        /// The consumer group, as written — see [`ConsumerSource`] for why this
        /// is never derived.
        group: String,
        /// How a message becomes fields, as the word a statement wrote.
        ///
        /// Carried as written for the reason a vector distance is: which formats
        /// exist is the store's question and not the grammar's, so an unknown
        /// one is refused where the store knows what it knows, with the span the
        /// author can see.
        format: Name,
        /// Which message field carries the record's identity.
        ///
        /// Required, and it is what makes a replayed message converge to one
        /// record rather than to two — the whole reason this node may claim
        /// at-least-once delivery with idempotent application.
        identity: FieldPath,
        /// Which message fields become which record fields.
        ///
        /// A field nobody named **does not land**. That is the anti-inference
        /// rule stated positively: a producer adding a field changes nothing
        /// here, where an inferred mapping would have started writing it.
        mapping: Vec<FieldMapping>,
        /// The table the records land in.
        destination: TableRef,
        /// What happens to a message that cannot be applied.
        on_failure: OnFailure,
        /// How many consumers this declaration runs.
        ///
        /// `None` reads as one, not as "decide for me".
        parallelism: Option<u32>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP CONSUMER orders_in` — stops it and forgets the declaration.
    DropConsumer {
        /// The name to remove.
        name: Name,
    },
    /// `DROP USER ada`
    DropUser {
        /// The name to remove.
        name: Name,
    },
    /// `GRANT read, write ON orders TO ada`
    ///
    /// # Grants, if a user has any, are the whole story
    ///
    /// A user with none is governed by their role, which is what lets this exist
    /// without changing what any already-declared user may do. A user with one
    /// reaches exactly what they were granted — because a role can only widen,
    /// and a permission system that cannot narrow is decoration.
    Grant {
        /// What may be done — one or more verbs, as written.
        verbs: Vec<Name>,
        /// The table it is on.
        table: TableRef,
        /// Which fields may be read, or empty for all of them.
        ///
        /// The same rule the grant has one level up: what is named is the whole
        /// story, and naming nothing names no limit.
        fields: Vec<Name>,
        /// Who it is for.
        user: Name,
    },
    /// `REVOKE write ON orders FROM ada`
    ///
    /// # It will not take away the last one
    ///
    /// Going from one grant to none **widens** a user from a named table to
    /// every table their role allows, which is the opposite of what somebody
    /// running a `REVOKE` is thinking about. So the last one is refused and the
    /// refusal says how to widen deliberately.
    Revoke {
        /// What is being taken away.
        verbs: Vec<Name>,
        /// The table it was on.
        table: TableRef,
        /// Who it was for.
        user: Name,
    },
    /// `DROP FIELD email ON users` — removes the declaration, not the data.
    DropField {
        /// The field's name.
        name: Name,
        /// The table it was declared on.
        table: TableRef,
    },
    /// `RELATE users:1->follows->users:2` / `… = { since: … }`
    Relate {
        /// The edge's source.
        from: RecordTarget,
        /// The edge table the relation is recorded in.
        edges: TableRef,
        /// The edge's target.
        to: RecordTarget,
        /// The edge's own properties, when the statement gives any.
        value: Option<Expr>,
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
    /// `REBUILD INDEX by_embedding ON papers`
    ///
    /// Makes the index's entries exactly what its table's rows imply, discarding
    /// whatever churn left behind. It is a statement rather than something the
    /// store decides for itself because two replicas must rebuild at the same
    /// point in the log; one that rebuilt on its own reckoning would answer an
    /// approximate question differently from its peers, and differ silently.
    RebuildIndex {
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
        /// The record to change.
        target: RecordTarget,
        /// How it changes.
        edit: Edit,
    },
    /// `DELETE users:1`
    Delete {
        /// The record to remove.
        target: RecordTarget,
    },
    /// `DELETE FROM readings WHERE at < datetime '…'` — every record a
    /// condition holds for.
    ///
    /// Separate from the single-record form rather than folded into it, because
    /// the two answer different questions and one of them can remove a table.
    /// `DELETE readings:1` says which record; this says which *kind*, and a
    /// statement that could mean either depending on a token is one a reader has
    /// to parse before they can review it.
    DeleteWhere {
        /// The table being cleared out.
        table: TableRef,
        /// What a record must satisfy to be removed.
        condition: Box<Expr>,
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
    /// `PUT media:'/logo.png' = 0x0a1b` — a file's whole content.
    ///
    /// One commit, so a half-written file is not a state this store can be in.
    Put {
        /// The file to write.
        target: RecordTarget,
        /// The byte offset to write at; absent replaces the whole file.
        ///
        /// `START` means here what it means over rows: skip this many. A write
        /// at an offset lands in **one** commit, the same as a whole-file one,
        /// so there is no moment at which a reader sees half of it.
        start: Option<u64>,
        /// Its bytes.
        value: Expr,
    },
    /// `READ media:'/logo.png'` — a file's bytes, or part of them.
    Read {
        /// The file to read.
        target: RecordTarget,
        /// The byte offset to read from; absent starts at the beginning.
        start: Option<u64>,
        /// How many bytes to answer with; absent reads to the end.
        limit: Option<u64>,
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

/// What an `INFO FOR` asks about.
///
/// Five subjects, and each one has **exactly one** rule deciding what the caller
/// may see. That is why they are five subjects rather than one with a filter
/// argument: a statement whose answer mixes two permission levels can only give
/// a partial answer or a confusing refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoSubject {
    /// `INFO FOR STORE` — the namespaces.
    Store,
    /// `INFO FOR NAMESPACE` — the databases in the selected namespace.
    Namespace,
    /// `INFO FOR DATABASE` — the tables in the selected database.
    Database,
    /// `INFO FOR TABLE users` — one table's shape, fields and indexes.
    Table(TableRef),
    /// `INFO FOR USER ada` — one user's role, tenancy and grants.
    ///
    /// The one subject that refuses rather than filters, because its content
    /// *is* the permission system: a partial view of who may do what is worse
    /// than none, since it reads as the whole answer.
    User(Name),
    /// `INFO FOR USERS` — the users of the tenancy the caller administers.
    ///
    /// The sixth subject, and it exists because the fifth cannot answer the
    /// question an operator actually has: `INFO FOR USER <name>` needs a name,
    /// and a name you have forgotten was, until this, unrecoverable from the
    /// store by any route at all.
    ///
    /// It **refuses rather than filters**, exactly as [`InfoSubject::User`] and
    /// [`InfoSubject::Node`] do. That is the whole reason it is safe to add: a
    /// listing narrowed to what a `viewer` may see would be a partial account of
    /// who may do what, and a partial account reads as the whole one. So it is
    /// answered only to a caller who administers the tenancy — and then it is
    /// answered in full for that tenancy, which is a different claim from a
    /// filtered view across tenancies the caller does not hold.
    ///
    /// It carries each user's name, role and tenancy, and **not their grants**.
    /// Grants are per-user detail and stay in `INFO FOR USER <name>`, where one
    /// subject is being examined rather than counted.
    Users,
    /// `INFO FOR NODE` — this node's own settings, and the peers it knows.
    ///
    /// The one subject that reads **two stores**: the local `META` keyspace and
    /// the replicated catalog. It answers them as two named groups rather than
    /// one flat object, because a reader has to be able to tell which fields
    /// would follow a backup and which would not — and flattening them would
    /// make that a thing you have to remember (ADR-0020 §3).
    ///
    /// Refuses rather than filters, for `INFO FOR USER`'s reason in a different
    /// key: it names no table, so a grant check would pass over it vacuously,
    /// and roles and endpoints have no smaller truthful form to hand a viewer.
    Node,
    /// `INFO FOR CONSUMER orders_in` — one consumer's declaration and its
    /// running state on **this** node.
    ///
    /// Two named groups rather than one flat object, for [`InfoSubject::Node`]'s
    /// reason: the declaration follows a backup and the running state does not,
    /// and flattening them would make that a thing you have to remember
    /// (ADR-0020 §3).
    ///
    /// It is also where the two refusals are reported — no exactly-once, no
    /// schema inference — because the loudest complaint about the system that
    /// has shipped this feature for years is that a consumer can be declared and
    /// not observed, and the second loudest is that its delivery guarantee is
    /// documented somewhere other than where a person configures it.
    Consumer(Name),
    /// `INFO FOR CONSUMERS` — every declared consumer, and whether it is running.
    Consumers,
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

/// A read of records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Which values each record answers with.
    pub projection: Projection,
    /// Which access path the statement resolves to.
    pub from: Source,
    /// The routes whose record references are followed before anything else
    /// looks at the record.
    ///
    /// Empty means the clause was not written. It is applied **before** the
    /// projection and the ordering, so `SELECT author.name … FETCH author` and
    /// `ORDER BY author.name` both see the record rather than the reference —
    /// which is the only ordering that makes the clause useful for the
    /// statements that want it.
    pub fetch: Vec<FieldPath>,
    /// The keys the records are grouped by, when the read groups.
    ///
    /// Empty means no grouping — which is not the same as no aggregate:
    /// `SELECT count(*) FROM users` folds every record into one group without
    /// naming a key, because the commonest question the language can be asked
    /// should not need a clause that means nothing.
    ///
    /// An **expression**, not a path, so a window is sayable:
    /// `GROUP BY time::bucket(at, 1h)`. A bare name still reads as a route into
    /// the record — the same reading `WHERE` and `ORDER BY` give it — so
    /// `GROUP BY city` means what it always did.
    pub group: Vec<Expr>,
    /// The keys the answer is sorted by, in order of significance.
    pub order: Vec<Ordering>,
    /// Whether the caller will accept an approximate ordering.
    ///
    /// **Permission, not a demand.** Every index in this store may change what a
    /// read costs and none may change what it answers — except a vector index,
    /// whose graph returns the neighbours a walk found and cannot show it missed
    /// none. So the exception is written in the statement: a read that does not
    /// say this gets the exact scan, and one that does may be served by the
    /// graph if there is one. With no such index it is still exact, which is
    /// better than what was asked for; the reported access path says which.
    pub approximate: bool,
    /// How many records to pass over before answering.
    pub start: Option<u64>,
    /// How many to answer with at most.
    pub limit: Option<u64>,
    /// Where the statement sits in the source.
    pub span: Span,
}

/// One sort key, and which way it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordering {
    /// What to sort by.
    ///
    /// Read in the condition position, so a bare name is a route into the
    /// record — or a projected name, since ordering runs after projection. An
    /// **expression** works too, which is what makes a nearest-neighbour query
    /// an ordinary `ORDER BY … LIMIT` rather than an operator of its own.
    pub key: Expr,
    /// Whether the order is reversed.
    pub descending: bool,
}

/// Which values a read answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// `*` — the record as it is stored.
    All,
    /// A named list, in the order it was written.
    ///
    /// Order is carried even though the answer is a name-ordered object, because
    /// an error naming the second of two colliding projections should point at
    /// the one the author wrote second.
    Values(Vec<Projected>),
}

/// One projected value, and the name it answers under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    /// How the value is produced.
    ///
    /// An ordinary expression, which is what makes `mean(age) * 2` writable: a
    /// fold is an [`ExprKind::Fold`] node like any other, so composing over one
    /// needs no second kind of projection. There is exactly one shape a fold
    /// has in this tree, which is the point — two would eventually disagree,
    /// and the disagreement would be a wrong number.
    pub value: Expr,
    /// The name it answers under.
    ///
    /// Resolved at parse rather than left for the executor: whether two
    /// projections collide is a property of the statement, so it is knowable
    /// before anything runs and is refused there.
    pub name: Name,
}

/// One step of a traversal: an edge table, and optionally the table its far
/// endpoint is read from.
///
/// Only the **last** step may leave the target out, and the grammar is what
/// guarantees that rather than a check: continuing a walk needs a node to
/// continue from, so `a->e1->e2` is one step landing on `e2` and never two steps
/// with a gap in the middle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// The edge table this step walks.
    pub edges: TableRef,
    /// The table the far endpoint is read from, when the statement names one.
    pub target: Option<TableRef>,
}

/// The three access paths the store has, named by what the statement targets.
///
/// Which one runs is decided here, by the shape of the statement, and not by a
/// cost model — there is no planner at this milestone and nothing pretends
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// This node's own identity.
    ///
    /// Its own variant and **not** a table, because its data is not records: it
    /// lives in the `META` keyspace so that it does not replicate, which is the
    /// one thing a system table cannot be (ADR-0018 §1, and §3's amendment,
    /// which corrects the sentence that said otherwise).
    ///
    /// It carries no [`TableRef`], and everything that decides permissions from
    /// the tables a statement names has to answer for that separately rather
    /// than reading an empty list as "nothing to check".
    Node,
    /// One record, by its identity.
    Record(RecordTarget),
    /// Every record of a table.
    Table(TableRef),
    /// A walk along one or more edge tables.
    ///
    /// `users:1->follows` reads the edge records themselves;
    /// `users:1->follows->users` resolves one step further and reads the records
    /// the edges point at; `users:1->follows->users->follows->users` does it
    /// again from there. Every step is an index read, because an edge table
    /// carries an index on each endpoint from the moment it is declared.
    Traverse {
        /// Where the walk starts.
        from: RecordTarget,
        /// Which way the arrows point.
        ///
        /// One direction for the whole walk. A per-hop direction asks a real
        /// question — "who follows somebody ada follows" — and is a separate
        /// design rather than a loosened rule; `docs/tessariql.md` §8 holds it.
        direction: Direction,
        /// The steps, in order. Never empty.
        hops: Vec<Hop>,
    },
    /// The records a condition holds for.
    ///
    /// Which access path this becomes is decided when it runs, by what exists:
    /// an index read where an index serves one of the condition's conjuncts, a
    /// scan where none does. The statement is the same either way, which is what
    /// lets an index be added later without rewriting a single query — and the
    /// candidates an index offers are still tested against the **whole**
    /// condition, because the index answered one conjunct and the statement
    /// asked for all of them.
    Where {
        /// The table being read.
        table: TableRef,
        /// What each record must satisfy.
        condition: Box<Expr>,
    },
    /// Two tables matched on a value neither of them stores a pointer for.
    ///
    /// The other kind of join from [`Select::fetch`]: a reference *is* an
    /// address, so following one is a point read, and this is for the
    /// relationship nobody wrote an address down for.
    ///
    /// # A row is a record with two named sides
    ///
    /// The answer is `{ users: { … }, orders: { … } }` rather than the two
    /// records merged. That is not a shape decision, it is the *naming* decision:
    /// merged records need a rule for what happens when both carry `name`, and
    /// every candidate rule — an alias syntax, a prefixing convention, last-wins
    /// — is something a reader has to learn. Nested, `users.name` and
    /// `orders.name` were never in danger of colliding, and every path,
    /// projection, `WHERE`, `ORDER BY` and `GROUP BY` works over it unchanged
    /// because it is an ordinary object.
    ///
    /// The row's identity is the **left** record's, so a left record matching
    /// two right records answers as two rows carrying one id. A row is not a
    /// record and this store's answers are keyed by record; a shape for rows is
    /// a change to the wire, the JSON surface and the console, which is a
    /// milestone rather than a clause.
    ///
    /// # Inner, so that `LEFT` stays additive
    ///
    /// A row appears only where both sides match. Decided now because it cannot
    /// be decided later: if a bare `JOIN` meant *outer*, adding `LEFT`
    /// afterwards would change what already-written statements answer.
    Join {
        /// The side that is read and drives.
        left: TableRef,
        /// The side that is probed.
        right: TableRef,
        /// The route into a left record whose value is matched.
        left_key: FieldPath,
        /// The route into a right record it is matched against.
        right_key: FieldPath,
        /// What each joined row must satisfy, when a `WHERE` was written.
        ///
        /// Over the **composite**, so it reads `users.name` and `orders.total`
        /// like everything else does.
        condition: Option<Box<Expr>>,
    },
}

/// An operator producing a number from two numbers.
///
/// Separate from [`BinaryOp`] for the same reason `AND` and `OR` are: these
/// answer with a number and can fail on their operands, while a comparison
/// answers with a boolean and is total in both arguments. One enum would leave
/// each implementation with arms it can never reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOp {
    /// `+` — addition. Numbers only; concatenation is `string::concat`.
    Add,
    /// `-` — subtraction.
    Subtract,
    /// `*` — multiplication.
    Multiply,
    /// `/` — division, which always produces at least a decimal.
    ///
    /// `7 / 2` is `3.5` and not `3`: truncating integer division is the classic
    /// silent wrong answer, where the query looks right and the number is not.
    Divide,
    /// `%` — remainder.
    Remainder,
}

impl ArithmeticOp {
    /// How the operator is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
        }
    }
}

pub use tessari_types::BinaryOp;

/// A value written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// Where it sits in the source.
    pub span: Span,
}

impl Expr {
    /// Whether two expressions say the same thing, wherever they were written.
    ///
    /// `==` compares spans, so two textually identical expressions in one
    /// statement are unequal — which is right for an AST and wrong for the one
    /// question a caller keeps needing to ask: *is this projection projecting
    /// that group key?* `GROUP BY time::bucket(at, 1h)` and the projection that
    /// names it are the same expression written twice, at two places.
    ///
    /// The walk compares kinds and recurses; a span never takes part.
    #[must_use]
    pub fn same_shape(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (ExprKind::Not(left), ExprKind::Not(right))
            | (ExprKind::Negate(left), ExprKind::Negate(right)) => left.same_shape(right),
            (ExprKind::And(a, b), ExprKind::And(c, d))
            | (ExprKind::Or(a, b), ExprKind::Or(c, d)) => a.same_shape(c) && b.same_shape(d),
            (
                ExprKind::Binary { op, left, right },
                ExprKind::Binary {
                    op: other_op,
                    left: other_left,
                    right: other_right,
                },
            ) => op == other_op && left.same_shape(other_left) && right.same_shape(other_right),
            (
                ExprKind::Arithmetic { op, left, right },
                ExprKind::Arithmetic {
                    op: other_op,
                    left: other_left,
                    right: other_right,
                },
            ) => op == other_op && left.same_shape(other_left) && right.same_shape(other_right),
            (
                ExprKind::Call {
                    function,
                    arguments,
                    ..
                },
                ExprKind::Call {
                    function: other_function,
                    arguments: other_arguments,
                    ..
                },
            ) => {
                function == other_function
                    && arguments.len() == other_arguments.len()
                    && arguments
                        .iter()
                        .zip(other_arguments.iter())
                        .all(|(left, right)| left.same_shape(right))
            }
            (ExprKind::Array(left), ExprKind::Array(right))
            | (ExprKind::Set(left), ExprKind::Set(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(one, two)| one.same_shape(two))
            }
            (ExprKind::Path(left), ExprKind::Path(right)) => left.path == right.path,
            (ExprKind::Literal(left), ExprKind::Literal(right)) => left == right,
            (ExprKind::Table(left), ExprKind::Table(right)) => left.name.text == right.name.text,
            // Everything else — a record target, a nested read, an object — is
            // compared as written, spans and all. Two nested reads that differ
            // only in where they sit are not a case this question is ever asked
            // about, and claiming they are the same would be a guess.
            (left, right) => left == right,
        }
    }
}

/// The expression forms this milestone accepts.
///
/// There is no arithmetic and no call yet: an expression is a value, a container
/// of expressions, a read, or a test built out of those. The reads are what make
/// a key-value value usable inside a record statement without either model
/// knowing about the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal that is already a whole value.
    Literal(Value),
    /// `$name` — a value the caller supplies when the script is run.
    ///
    /// Present only between parsing and binding: [`Script::bind`] replaces every
    /// one of these with the [`ExprKind::Literal`] it is bound to, so nothing
    /// downstream — the planner, the index chooser, the evaluator — ever meets
    /// one. That is the whole design: substitution happens **after** parsing, so
    /// there is no stage left at which a supplied value could be read as
    /// grammar.
    Parameter(String),
    /// A value read out of the record being tested: `name`, `address.city`.
    ///
    /// Only meaningful where there **is** a record — inside a condition. In a
    /// value position a bare name is a table, which is why the two positions
    /// read the same token differently and why the parser knows which it is in.
    Path(FieldPath),
    /// `NOT <expr>` — the operand must be a boolean.
    Not(Box<Expr>),
    /// `-<expr>` — the operand must be a number.
    Negate(Box<Expr>),
    /// `count(*)`, `mean(age)` — a fold over the records of a group.
    ///
    /// An expression node rather than a projection of its own, so a fold
    /// composes: `mean(price) * 1.2` is arithmetic whose left operand happens to
    /// collapse many records into one.
    ///
    /// It is legal only in a projection, and only in a read that groups —
    /// which is every read that contains one, since a fold is what makes a read
    /// grouped. In a condition it is refused and the refusal names what it would
    /// be: a filter over groups is `HAVING`, which has its own scoping rule and
    /// its own row in the specification's list of absences.
    ///
    /// Its value is **constant within a group**, and the evaluator makes that
    /// literally true rather than claiming it: the fold is computed once per
    /// group and substituted into the tree as a literal before the enclosing
    /// expression is evaluated.
    Fold {
        /// Which fold.
        fold: Aggregate,
        /// What it folds over — absent for `count(*)`, which folds over the
        /// records themselves rather than over a value in them.
        over: Option<Box<Expr>>,
        /// Where it was written.
        span: Span,
    },
    /// `group::name(a, b)` — a call of one of the language's own functions.
    ///
    /// Arity is checked when the statement is read, because the set of
    /// functions is known then; argument types are checked when it runs,
    /// because until a record is in hand there is nothing to check.
    Call {
        /// Which function.
        function: Function,
        /// Its arguments, in order.
        arguments: Vec<Expr>,
        /// Where the name was written, for a failure to point at.
        span: Span,
    },
    /// `<expr> AND <expr>` — both must be booleans, and the right is evaluated
    /// only when the left holds.
    ///
    /// Separate from [`ExprKind::Binary`] rather than an operator inside it,
    /// because composing conditions and comparing values are genuinely
    /// different: one short-circuits and demands booleans, the other takes any
    /// two values and always looks at both. Folding them together would leave
    /// every value-level operator with two arms it can never reach.
    And(Box<Expr>, Box<Expr>),
    /// `<expr> OR <expr>` — the right is evaluated only when the left does not
    /// hold.
    Or(Box<Expr>, Box<Expr>),
    /// Two numbers and an operator.
    Arithmetic {
        /// Which operator.
        op: ArithmeticOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// Two values and an operator.
    Binary {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
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

/// How a record target names the record.
///
/// A record is `table:id`, and the two halves are different things: the table is
/// a **name**, which a caller may never supply, and the id is a **value**, which
/// they may. So a parameter stands here and nowhere else in an identity —
/// `GET sessions:$token` is the shape a key-value read actually has, and
/// building it as text is the string-building parameters exist to remove.
///
/// [`Script::bind`] replaces every [`Identity::Parameter`] with the value it is
/// bound to, so nothing past binding meets one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Written in the statement.
    Fixed(RecordId),
    /// Supplied by the caller.
    Parameter(String),
}

impl Identity {
    /// The identity, once it is one.
    ///
    /// # Errors
    ///
    /// [`crate::Error::UnboundParameter`] when a parameter reaches execution,
    /// which binding makes unreachable — so this says the script was run without
    /// being bound rather than guessing at a value.
    pub fn fixed(&self, span: Span) -> crate::Result<&RecordId> {
        match self {
            Self::Fixed(id) => Ok(id),
            Self::Parameter(name) => Err(crate::Error::UnboundParameter {
                name: name.clone(),
                span,
            }),
        }
    }
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

/// A fold over the records of a group.
///
/// Spelled without a namespace, where every function has one. That is the
/// namespacing rule earning its keep rather than being broken by it:
/// `array::len` counts one record's array and `count` counts records, and a
/// reader can tell which arity they are looking at from the spelling alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// `count(*)` counts records; `count(<expr>)` counts the records where the
    /// expression is present and not null.
    Count,
    /// `sum(<expr>)` — over nothing, zero.
    Sum,
    /// `mean(<expr>)` — over nothing, `NONE`.
    Mean,
    /// `min(<expr>)`, in the value system's order.
    Min,
    /// `max(<expr>)`, in the same order.
    Max,
}

impl Aggregate {
    /// Every fold, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Count, Self::Sum, Self::Mean, Self::Min, Self::Max];

    /// How the fold is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// The fold a word spells, if it spells one.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|fold| fold.spelling().eq_ignore_ascii_case(word))
    }
}

/// An expression kept as the text it was written as.
///
/// The parser validates it by parsing it and then keeps the **source slice**,
/// because that is what the catalog stores: a definition should stay legible in
/// a dump, and a printer that rebuilt the text from the tree would be a second
/// grammar to keep in step with the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The text, exactly as the author wrote it.
    pub text: String,
    /// Where it sits in the source.
    pub span: Span,
}

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

/// A password as written, which prints as `<redacted>` and nothing else.
///
/// [`render`](crate::render) refuses to turn `DEFINE USER` back into text, so
/// that a credential cannot be recovered from a statement the store is holding.
/// A `String` field inside a derived `Debug` gives it back in one
/// interpolation, and the line that does it is always somewhere else and
/// written later — the same reasoning `tessari-wire` writes out over its own
/// hand-written `Debug` for a request.
///
/// The plaintext is reachable only through [`Password::expose`], so every place
/// that reads it is a place somebody chose to write that name.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// Hold a password the parser has just read.
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self(text)
    }

    /// The plaintext, for the one caller that hashes it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The one field an [`AlterUser`](StatementKind::AlterUser) statement changes.
///
/// One per statement rather than a record of optional fields, because the
/// difference matters at the point of writing: a struct of `Option`s makes
/// "leave the password alone" and "set the password to nothing" the same shape,
/// and the executor then has to be trusted to tell them apart. Here the
/// statement carries only what it came to change, and nothing else can be
/// touched by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserChange {
    /// `SET PASSWORD '…'` — a new credential, hashed before it is stored.
    Password(Password),
    /// `SET ROLE editor` — what the user may do, within the tenancy they
    /// already hold. The tenancy itself does not move.
    Role(Name),
}

/// Which endpoint of an edge a traversal starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `->` — the walk starts at the edge's `out` and arrives at its `in`.
    Outgoing,
    /// `<-` — the walk starts at the edge's `in` and arrives at its `out`.
    Incoming,
}

impl Direction {
    /// The endpoint field a walk in this direction matches on.
    #[must_use]
    pub const fn from_field(self) -> &'static str {
        match self {
            Self::Outgoing => "out",
            Self::Incoming => "in",
        }
    }

    /// The endpoint field a walk in this direction arrives at.
    #[must_use]
    pub const fn to_field(self) -> &'static str {
        match self {
            Self::Outgoing => "in",
            Self::Incoming => "out",
        }
    }
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
    pub id: Identity,
    /// Where the whole reference sits.
    pub span: Span,
}
