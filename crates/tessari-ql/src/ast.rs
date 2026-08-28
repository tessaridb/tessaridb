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

use tessari_types::{Assertion, Duration, FieldKind, Filter, Path, RecordId, Value};

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
        ///
        /// Boxed because this variant is the widest in the enum and every
        /// statement anywhere is sized by it.
        value: Option<Box<Expr>>,
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
        /// What the statement answers with. `BEFORE` is refused: there was no
        /// record before, and a statement that answered `NONE` to a question
        /// somebody meant would be worse than one that says the question does
        /// not apply.
        answer: Answer,
    },
    /// `SELECT * FROM …`
    Select(Select),
    /// `UPDATE users:1 = { … }` — the value is replaced, never merged.
    Update {
        /// The record to change.
        target: RecordTarget,
        /// How it changes.
        edit: Edit,
        /// What the statement answers with.
        answer: Answer,
    },
    /// `THROW 'this order is already paid'` — refuse the script.
    ///
    /// With `IF` in the language a script can compute a decision and, until
    /// this, could not act on it: every refusal had to be a condition the store
    /// itself happened to check. The statement never answers — it fails, and a
    /// failure inside a transaction discards the work above it, which is the
    /// behaviour a guard clause needs to be worth writing.
    Throw {
        /// The message. Evaluated, so it may name what went wrong.
        value: Expr,
    },
    /// `UPSERT users:1 = { … }` — write the record whether or not it is there.
    ///
    /// Its own statement rather than a flag on `UPDATE`, because the three verbs
    /// assert three different things about the record before the write:
    /// `CREATE` says it is absent, `UPDATE` says it is present, and this one
    /// says nothing. A caller who knows which case they are in keeps the
    /// refusal that tells them when they were wrong.
    Upsert {
        /// The record to write.
        target: RecordTarget,
        /// How it is written. A record that is not there starts as an empty
        /// object, so `SET` and `MERGE` mean the same thing over an absence
        /// that they mean over a record with none of the named routes.
        edit: Edit,
        /// What the statement answers with. `BEFORE` over a record that was not
        /// there answers `NONE`, which is the true answer rather than a silent
        /// one — the caller asked what was there, and nothing was.
        answer: Answer,
    },
    /// `DELETE users:1`
    Delete {
        /// The record to remove.
        target: RecordTarget,
        /// What the statement answers with. `AFTER` is refused: there is no
        /// record after a delete, so the clause could only ever answer `NONE`.
        answer: Answer,
    },
    /// `DELETE FROM readings WHERE at < datetime '…' LIMIT 100` — the records a
    /// condition holds for, up to a stated bound.
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
        /// How much this statement may remove.
        ///
        /// Not an `Option`. A bound that could be absent would let the
        /// unbounded form exist in the tree, and the whole point of the clause
        /// is that removing a table has to be *said*.
        limit: DeleteBound,
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
    /// `LET $recent = SELECT id FROM notes ORDER BY at DESC LIMIT 5`
    ///
    /// # Why this is what makes the engines compose
    ///
    /// A read resolves to exactly one access path — a table, a record, an index,
    /// a walk — chosen by the shape of the statement. That is what keeps the
    /// cost of a read legible, and it is also why a question that crosses two
    /// engines could not be *said*: the nearest neighbours by embedding, and
    /// then who wrote them, is a vector read followed by a graph walk, and there
    /// was no way for the first answer to reach the second statement.
    ///
    /// A binding is that way, and it needs no planner: each statement still
    /// resolves to one path, and what travels between them is a value.
    ///
    /// # The value is substituted, not looked up
    ///
    /// When this statement runs, the value it produced replaces every mention of
    /// its name in the statements that have **not run yet** — the same walk
    /// [`Script::bind`](crate::Script::bind) performs for a caller's parameters,
    /// for the same reason. The planner reads an expression tree to find a
    /// right-hand side an index can serve, so a name it could not resolve would
    /// silently drop the index for exactly the reads this feature exists to
    /// enable. After the substitution there is no name left to resolve.
    Let {
        /// The name, without its `$`.
        name: String,
        /// The value to bind.
        value: Expr,
        /// Where the name was written, for the error that says it was bound
        /// twice.
        span: Span,
    },
    /// `RETURN { total: $sum, seen: $count }`
    ///
    /// Names the value the script answers with. A script may hold **at most
    /// one**, refused at parse where there are two — a second would make "the
    /// answer" depend on which one ran, which is a question no reader should
    /// have to ask of a script they are looking at.
    ///
    /// It does not end the script. Ending it would make everything below
    /// unreachable, and unreachable statements inside a `BEGIN`/`COMMIT` would
    /// leave the transaction open.
    Return {
        /// What to answer with.
        value: Expr,
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

/// A read of records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Which values each record answers with.
    pub projection: Projection,
    /// The routes `*` must not contribute, when `OMIT` named any.
    ///
    /// Empty means the clause was not written. It subtracts from what the star
    /// put there and from nothing else: a value written out by name was asked
    /// for explicitly, so removing it would be answering a question nobody
    /// asked, and the parser refuses the clause where there is no star to
    /// subtract from rather than accepting one that does nothing.
    ///
    /// A route rather than a name, because the field to leave out may be inside
    /// the record: `OMIT address.postcode` keeps the address.
    pub omit: Vec<FieldPath>,
    /// Which access path the statement resolves to.
    pub from: Source,
    /// Where `ONLY` was written, when it was.
    ///
    /// The clause is an **assertion by the author** that at most one record
    /// answers, and the answer is shaped to match: the record itself rather than
    /// a list holding it, so that a caller reading one thing does not unwrap a
    /// list of one everywhere.
    ///
    /// Its span rather than a bare `bool` because the refusal points at the word
    /// — the author either meant a different read or did not mean the word, and
    /// both are decided by looking at it.
    ///
    /// Nothing about it is checked before the read runs. A parse-time rule would
    /// have to say which sources can answer with one, and it cannot know:
    /// `FROM ONLY users WHERE email = $e` is the commonest correct use of the
    /// clause and carries no bound the parser can see, because the uniqueness it
    /// rests on lives in the schema and in the data.
    pub only: Option<Span>,
    /// The routes whose record references are followed before anything else
    /// looks at the record.
    ///
    /// Empty means the clause was not written. It is applied **before** the
    /// projection and the ordering, so `SELECT author.name … FETCH author` and
    /// `ORDER BY author.name` both see the record rather than the reference —
    /// which is the only ordering that makes the clause useful for the
    /// statements that want it.
    pub fetch: Vec<FieldPath>,
    /// The route whose array is opened into one record per element, when
    /// `SPLIT ON` named one.
    ///
    /// A route rather than a name, like `OMIT` and `FETCH`, so the array may be
    /// inside the record: `SPLIT ON address.tags`.
    ///
    /// Applied **after** `FETCH` and before everything that groups, projects or
    /// sorts — after the fetch because a reference resolved once and then opened
    /// is the same answer as one opened and then resolved n times, and before
    /// the rest because every one of them counts records and the split is what
    /// decides how many there are.
    ///
    /// One route and not a list. Two would be a cartesian product, which is a
    /// different question and should have to say so.
    pub split: Option<FieldPath>,
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
    /// The record the answer resumes after, when `AFTER` named one.
    ///
    /// A cursor: the page begins at the first record that sorts **strictly
    /// after** this one in the answer's own order. A record identity rather than
    /// an opaque token because the caller already holds it — the answer carries
    /// the identity of every record in it — so the clause needs no new return
    /// channel, no token format, and no version of one.
    ///
    /// **It supplies the order it resumes.** With an `ORDER BY` that is the
    /// order written; with none, it is the store's own key order, which is why a
    /// cursor read that names no order still answers identity-ascending rather
    /// than in whatever order the source happened to produce. A cursor without
    /// an order to resume would be a filter on a sequence nobody promised.
    ///
    /// A `START` beside it is refused where the statement is read: an offset and
    /// a cursor are two answers to the same question, and accepting both would
    /// make one of them silently lose.
    ///
    /// Boxed where every other field of this struct is inline, because a record
    /// target is one of the larger things the grammar holds and a cursor is
    /// absent from very nearly every statement ever parsed. Inline it made
    /// `Select` the outlier variant of [`Statement`] — every statement of every
    /// kind paying for a clause almost none of them write.
    pub after: Option<Box<RecordTarget>>,
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
    /// What the author expects the read to have done, when they said.
    ///
    /// `None` is the ordinary case: the statement asks a question and the store
    /// answers it however it can.
    pub using: Option<Using>,
    /// How long the read may take before it is refused.
    ///
    /// `None` is the ordinary case: a read takes as long as it takes.
    pub timeout: Option<Timeout>,
    /// Where the statement sits in the source.
    pub span: Span,
}

/// A ceiling on how long a read may run.
///
/// **Refused, never truncated.** A read that reaches its ceiling fails; it does
/// not answer with the part it had. A partial answer that looks whole is the
/// failure this store spends its rules removing, and a timeout is the easiest
/// place in a language to introduce one — the records are already in hand and
/// returning them costs nothing, which is exactly why it must not be done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout {
    /// The ceiling, as the statement wrote it.
    pub after: Duration,
    /// Where the clause sits, for the refusal to point at.
    pub span: Span,
}

/// An assertion about how a read was served.
///
/// **A refusal, never a router.** It does not choose a path — nothing here
/// reaches the planner — it fails the statement when the path taken is not the
/// one named. That turns the worst failure mode an indexed store has, the query
/// that quietly stops using its index and starts scanning, from a thing you find
/// out from a latency graph into a thing the statement says out loud.
///
/// It is checked against what the read **did**, not against what the planner
/// chose, and the difference matters: an ordered index that could not fill the
/// bound sends the read to the scan, and an assertion satisfied by the planner's
/// intention would pass exactly where the scan it was written to catch happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Using {
    /// `USING <path>` — the access path the read is expected to report.
    ///
    /// Carried as written rather than as a checked variant, because the set of
    /// path words belongs to the store that reports them and duplicating it in
    /// the grammar would be a second vocabulary of exactly the kind one plan
    /// structure exists to remove. An unrecognised word is refused before the
    /// read runs, naming the ones that exist.
    Path(Name),
    /// `USING INDEX <name>` — the index the read is expected to have used.
    ///
    /// Stronger than a path word and often what is actually meant: `index` says
    /// *an* index answered, this says *which*.
    Index(Name),
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
    /// `*` — the record as it is stored, with nothing added.
    ///
    /// Kept apart from the composed form below rather than expressed as one of
    /// its cases, because it is the read that copies nothing: the records go to
    /// the answer as they were decoded, and giving it a projection to apply
    /// would give the commonest read in the language a per-record allocation to
    /// pay for a list that is empty.
    All,
    /// A named list, in the order it was written, and whether the record's own
    /// fields join them.
    ///
    /// Order is carried even though the answer is a name-ordered object, because
    /// an error naming the second of two colliding projections should point at
    /// the one the author wrote second. It is also why `*` needs no position of
    /// its own here: the answer is ordered by name whatever order the list was
    /// written in, so *where* the star stands among the values cannot be
    /// observed.
    Values {
        /// `*` written among the values, if it was.
        ///
        /// The span is what a message about a field the record and the list both
        /// name would point at.
        everything: Option<Span>,
        /// The values written out, each with the name it answers under.
        values: Vec<Projected>,
    },
}

impl Projection {
    /// Whether the record's own fields reach the answer.
    ///
    /// The one question two clauses ask of a projection — `OMIT`, which has
    /// nothing to subtract from without it, and the projection stage, which
    /// starts from the record rather than from nothing.
    #[must_use]
    pub const fn stars(&self) -> bool {
        match self {
            Self::All => true,
            Self::Values { everything, .. } => everything.is_some(),
        }
    }

    /// The values written out by name, which is none for a bare `*`.
    #[must_use]
    pub fn written(&self) -> &[Projected] {
        match self {
            Self::All => &[],
            Self::Values { values, .. } => values,
        }
    }
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
        ///
        /// Boxed, with the other side, because a side may itself hold a whole
        /// read: inline they make this variant three times the size of every
        /// other one, and every [`Source`] anywhere pays for it.
        left: Box<JoinSide>,
        /// The side that is probed.
        right: Box<JoinSide>,
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
    /// A read whose answer is what the outer statement reads.
    ///
    /// The answer is the inner records themselves — not wrapped, not renamed —
    /// so an outer `WHERE`, `ORDER BY` and projection read them exactly as they
    /// would read the table. That is what makes one engine's answer the next
    /// question's source without a shape to learn.
    ///
    /// # It states its own ceiling
    ///
    /// The inner read is **materialised**: unlike a table, there is no index to
    /// walk and no bound to push down, so every record it answers with is held
    /// at once. A source that could grow without limit is therefore refused
    /// without a `LIMIT`, rather than truncated at a number nobody wrote — a
    /// silently truncated source answers a different question from the one that
    /// was asked, and looks exactly like a complete one.
    ///
    /// # It is also the only place some reads can be filtered
    ///
    /// `WHERE` belongs to the table position — `FROM t WHERE c` — so a
    /// traversal and a grouped read have nowhere to put one. Wrapping either in
    /// a materialised read gives it one, which is why the condition lives here
    /// rather than only inside. It is also where a condition over a *projected*
    /// name goes: `SELECT city, count(*) AS n … GROUP BY city` produces `n`, and
    /// nothing inside that read can ask about it.
    Subquery {
        /// The read whose answer this source is.
        read: Box<Select>,
        /// What each of its records must satisfy, when a `WHERE` was written.
        condition: Option<Box<Expr>>,
    },
}

/// One side of a join — what it reads, and the name the row files it under.
///
/// The name is not decoration. A row is `{ <left>: …, <right>: … }`, so the two
/// sides need two names, and `ON`, `WHERE`, `ORDER BY` and the projection are
/// all routes through them. A table brings its own name and may be given
/// another; a read brings none, which is why the two cases are different
/// variants rather than one variant with an optional name that is sometimes
/// required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinSide {
    /// A table, under its own name unless `AS` gave it another.
    ///
    /// An alias is what makes a self-join sayable: `users AS a JOIN users AS b`
    /// files two records of one table under two names, where `users JOIN users`
    /// has one name for both and no row a reader could address.
    Table {
        /// The table read.
        table: TableRef,
        /// The name `AS` gave it, when one was written.
        alias: Option<Name>,
    },
    /// A read, materialised and then joined, under the name `AS` gave it.
    ///
    /// The name is **mandatory** and typed as such: a read has no name of its
    /// own, and a side with no name has no place in the row.
    Read {
        /// The read whose answer this side joins.
        read: Box<Select>,
        /// The name the row files it under.
        alias: Name,
    },
}

impl JoinSide {
    /// The name this side answers under in the row.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Table {
                alias: Some(alias), ..
            }
            | Self::Read { alias, .. } => &alias.text,
            Self::Table { table, alias: None } => &table.name.text,
        }
    }
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
    /// `IF <test> THEN <a> ELSE <b> END` — a value that depends on a test.
    ///
    /// An expression rather than a statement, deliberately: what was missing was
    /// not control flow but the ability to *compute* a value conditionally — in
    /// a projection, an assignment, a filter, an ordering. A statement form
    /// would have served none of those positions.
    ///
    /// Without an `ELSE` the answer is `NONE`, which is what a path into a field
    /// the record does not have already answers — so the two absences compose
    /// rather than needing a rule apiece.
    If {
        /// The test, which must answer with a boolean.
        condition: Box<Expr>,
        /// The value when it holds.
        then: Box<Expr>,
        /// The value when it does not; absent means `NONE`.
        otherwise: Option<Box<Expr>>,
    },
    /// `a ?? b` — the left value unless it holds nothing.
    ///
    /// "Holds nothing" is `NONE` **or** `NULL`, and this is the one place the
    /// language treats the two alike. It is the right place: the question `??`
    /// asks is *"is there a value here for me to use"*, and the answer is no in
    /// both cases. Everywhere else keeps them apart, which is why `= NONE` and
    /// `= NULL` remain different questions.
    ///
    /// The right side is evaluated **only** when the left holds nothing, so
    /// `cached ?? (SELECT …)` does not pay for a read it does not need.
    Coalesce(Box<Expr>, Box<Expr>),
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
