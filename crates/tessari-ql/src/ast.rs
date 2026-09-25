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

use tessari_types::{
    Assertion, ConflictPolicy, Duration, FieldKind, Filter, IdentityKind, Path, RecordId,
    Replication, ReplicationClass, Value,
};

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
        /// Who this session is, when it claims from a queue.
        ///
        /// A **string literal** and not an identifier, because it is data the
        /// client chose rather than a catalog object — and deliberately not
        /// declared anywhere first, so a worker starting under autoscale needs
        /// no `DEFINE` before it can work.
        ///
        /// The same name on several sessions means **share the work**, which is
        /// what a single declared name reads as. Nothing is fenced: sharing is
        /// the point, and the identity that must not collide is the instance the
        /// engine mints beside this, never this.
        consumer: Option<String>,
    },
    /// `DEFINE NAMESPACE prod REPLICATION FACTOR 3`
    DefineNamespace {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
        /// How many copies the cluster is asked to keep, when the statement
        /// said.
        ///
        /// `None` is a namespace that **said nothing**, which is not the same
        /// as [`Replication::None`] and is deliberately not defaulted to it
        /// (ADR-0060). The difference is the whole point of carrying the
        /// clause this early: a namespace that declined replication is
        /// honoured, and a namespace nobody asked is refused at the moment a
        /// second node would hold it. Collapsing the two here would make that
        /// distinction unrecoverable, because by then the namespaces exist.
        ///
        /// The clause is **optional today and mandatory later**. ADR-0060 asks
        /// for mandatory, and this is a sequencing departure recorded in W211's
        /// plan rather than a reversal: a clause made mandatory before there
        /// are nodes to place copies on could only be answered with `NONE`,
        /// which trains an operator to decline without thinking — the
        /// inherited default the ADR exists to abolish, wearing a costume.
        replication: Option<Replication>,
        /// How many writers it admits, when the statement said (G027 S2.1).
        ///
        /// `None` is a namespace that **said nothing**, and that reads as
        /// single-leader wherever it is asked — which is what this engine has
        /// always done and what the two-leaderships refusal has always
        /// enforced. It is kept apart from a stated
        /// [`ReplicationClass::SingleLeader`] because an operator who answered
        /// the question has told the cluster something a silence has not.
        class: Option<ReplicationClass>,
    },
    /// `DEFINE DATABASE orders`
    DefineDatabase {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE TABLE users SCHEMAFULL` / `DEFINE TABLE follows EDGE`
    ///
    /// With columns: `DEFINE TABLE users (name string REQUIRED, age int)`.
    DefineTable {
        /// The name to create.
        name: Name,
        /// The fields declared with the table, in the order they were written.
        ///
        /// Empty for the flag-only spelling, which is not the same statement
        /// with nothing in its parentheses: `DEFINE TABLE t ()` is refused,
        /// because a reader writing empty parentheses meant to write something.
        columns: Vec<ColumnDeclaration>,
        /// Whether the table refuses a field it does not declare.
        schemafull: bool,
        /// Whether the table holds edges, and the pair it joins when it says
        /// so: `DEFINE TABLE follows EDGE FROM users TO users`.
        edge: Option<EdgeClause>,
        /// What the table names a record with when the caller does not:
        /// `DEFINE TABLE sessions IDENTITY uuid`.
        ///
        /// A property of the table and not of the write, because two records in
        /// one table named on two schemes sort into two regions of the keyspace
        /// and read back as one table only by accident.
        identity: IdentityKind,
        /// The graph the table belongs to: `DEFINE TABLE person IN social`.
        ///
        /// A clause rather than a word, because a node kind is a table in every
        /// respect that matters and differs by exactly this one fact (Q-314).
        /// An edge kind is the asymmetric case and gets its own word, because it
        /// is never selected from and its entries are not records.
        graph: Option<Name>,
        /// Where the table's shards begin: `DEFINE TABLE orders IDENTITY uuid
        /// SPLIT AT 'g', 'p'` is three shards (G031, ADR-0080).
        ///
        /// In the order the statement wrote them. Empty is one shard — a table
        /// that says nothing is not split. Whether the points are in key order is
        /// the catalog's to refuse rather than this list's to fix, because a list
        /// the parser sorted would hide the mistake the refusal names.
        split: Vec<RecordId>,
        /// What the table does with a write it cannot order, when the statement
        /// said: `DEFINE TABLE ledger (…) LAST WRITER WINS` (G027 S3.2).
        ///
        /// `None` is a table that **said nothing**, which reads as a refusal
        /// wherever it is asked — what ADR-0075 has every table do. It is kept
        /// apart from a stated [`ConflictPolicy::Refuse`] because an operator
        /// who answered the question has told the cluster something a silence
        /// has not.
        ///
        /// On the table rather than the namespace, where the replication class
        /// sits, because a counter can tolerate a dropped update beside a ledger
        /// row in the same namespace that cannot (Q-633).
        conflict: Option<ConflictPolicy>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE GRAPH social` — the structure node tables belong to.
    ///
    /// The word names an **object**, which is the whole of what it adds: before
    /// it, a graph was a fact in somebody's head about which tables were
    /// related, so nothing could enumerate it, drop it, or be asked a question
    /// about it. A bounded walk needs a boundary and a question about the whole
    /// needs a whole to name, and this is where both come from.
    DefineGraph {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE EDGE works_at IN social FROM person TO company` — a join a graph
    /// writes adjacency under.
    ///
    /// A **word** rather than a clause, and the asymmetry with node membership is
    /// deliberate. A node kind *is* a table — selected from, inserted into,
    /// indexed, granted on — differing by exactly one fact, so it takes the
    /// clause `IN <graph>` on `DEFINE TABLE`. An edge kind is never selected
    /// from: its entries are adjacency keys held beside the node, so a hop is one
    /// range read rather than an index probe and a random read per neighbour.
    /// That is a difference large enough to earn a word of its own, and it is why
    /// the word could not ship before the adjacency it names.
    ///
    /// Both endpoint tables must belong to the same graph. That is what bounds a
    /// walk: a traversal cannot leave the graph through a join whose far side was
    /// never part of it.
    DefineEdge {
        /// The name to create.
        name: Name,
        /// The graph it belongs to.
        graph: Name,
        /// The table an edge of this kind leaves.
        from: Name,
        /// The table an edge of this kind reaches.
        to: Name,
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
    /// `DEFINE BUCKET media MAX 5242880` — a table whose records are files.
    ///
    /// The bytes live in a companion table nothing can name, and the records
    /// here are metadata the store fills in (ADR-0011). Declared with its own
    /// word rather than a flag on `DEFINE TABLE`, because what a caller may do
    /// to it differs: a bucket is written through `PUT` and never by hand.
    DefineBucket {
        /// The name to create.
        name: Name,
        /// The largest file the bucket accepts, in bytes, if one was declared.
        ///
        /// Written as a plain count of bytes rather than as `5MB`, because
        /// digits touching a letter are a **duration** in this grammar —
        /// whatever the letter — so `5MB` lexes as a duration with a unit
        /// nothing recognises and is refused. That rule is deliberate and
        /// belongs to the whole language; changing it to give one clause a
        /// shorter spelling would be a lexical change everywhere to buy a
        /// convenience here.
        ///
        /// Optional, and absent means unbounded — so every bucket declared
        /// before the clause existed keeps parsing and keeps its meaning, the
        /// same contract the edge table's endpoint pair keeps.
        max: Option<u64>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE COLLECTION notes` — records that carry fields nobody declared.
    ///
    /// The fourth word in the row `TABLE`, `SPACE`, `BUCKET` already forms, and
    /// it earns one on the same test they did: a difference in what a caller
    /// may **do**. A table refuses a field it does not declare; a collection
    /// accepts one, which is the whole of what a document is here.
    ///
    /// It carries no columns and no strictness marker, because there is nothing
    /// for either to say. `DEFINE TABLE t (…) SCHEMALESS` is a *table* whose
    /// declared fields are still constrained; a collection declares none. The
    /// two are stored apart rather than collapsed, so `INFO` can answer with the
    /// word that created the thing instead of one that merely behaves like it.
    DefineCollection {
        /// The name to create.
        name: Name,
        /// What the collection names a record with when the caller does not:
        /// `DEFINE COLLECTION sessions IDENTITY uuid`.
        identity: IdentityKind,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine` — a store whose
    /// records are vectors.
    ///
    /// The fifth word in the row `TABLE`, `SPACE`, `BUCKET`, `COLLECTION` forms,
    /// and it earns one on the same test they did. Written out as the three
    /// statements it stands for, a vector store is:
    ///
    /// ```text
    /// DEFINE COLLECTION embeddings;
    /// DEFINE FIELD vector ON embeddings TYPE vector<768> REQUIRED;
    /// DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine;
    /// ```
    ///
    /// Three statements a reader must get right *together*: a width without an
    /// index is a declaration nothing searches, an index without a width is the
    /// hole `vector<n>` was added to close, and either without `REQUIRED` admits
    /// a record with no vector at all — legal in a table, and not a record of a
    /// vector store. The word makes the three inseparable, which is a difference
    /// in what a caller may do rather than a shorter way to say the same thing.
    ///
    /// It **desugars** into exactly those three, through the same functions
    /// `DEFINE TABLE t (…)` desugars through. That is deliberate and it is the
    /// point: there is no store-only path to disagree with the field one,
    /// because the store's path *is* the field one.
    DefineVector {
        /// The name to create.
        name: Name,
        /// How wide every vector in the store is.
        ///
        /// Required, with no default, because declaring it is the whole
        /// capability: undeclared, a 512-wide row and a 768-wide row sit
        /// together legally and only the distance function notices — per read,
        /// long after the bad write.
        dimension: usize,
        /// The distance its index is built and searched with.
        ///
        /// Carried as the word the author wrote rather than as a parsed kind,
        /// for the reason [`StatementKind::DefineIndex`] carries it that way:
        /// which distances exist is the store's question, not the grammar's.
        distance: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE GEO places`
    ///
    /// The sixth word in the row, and it earns one on the same test the fifth
    /// did. Written out as the three statements it stands for, a geo store is:
    ///
    /// ```text
    /// DEFINE COLLECTION places;
    /// DEFINE FIELD geometry ON places TYPE geometry REQUIRED;
    /// DEFINE INDEX geometry ON places FIELDS geometry SPATIAL;
    /// ```
    ///
    /// Three statements that must be got right together: a geometry field with
    /// no spatial index makes every place query a scan, a spatial index with no
    /// declared field indexes nothing, and either without `REQUIRED` admits a
    /// record with no geometry — legal in a table, and not a record of a place
    /// store. It desugars into exactly those three, through the same functions
    /// [`StatementKind::DefineVector`] desugars through, so there is no
    /// store-only path that could come to disagree with the field one.
    ///
    /// **It takes no clause**, which is where the analogy with `DEFINE VECTOR`
    /// stops. A width has to be declared because nothing else refuses a row of
    /// the wrong shape; a geometry does not, because the read that needs a point
    /// already refuses everything else where it happens. Narrowing the store to
    /// one shape would also make a table of regions inexpressible, and regions
    /// are served correctly today (Q-324).
    DefineGeo {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE VAULT team` — a store whose fields can be sealed.
    ///
    /// A name and nothing else, like [`StatementKind::DefineGeo`]: what makes a
    /// vault a vault is the key it is created with, and a key is not something
    /// a caller supplies or chooses. The statement generates one, wraps it under
    /// the store's master key, and puts the wrapped bytes on the declaration —
    /// which is why it is the one declaration that **requires an unsealed
    /// store**, and refuses rather than creating a vault whose key would have to
    /// be invented later.
    ///
    /// Dropping it destroys that key, and destroying the key is the deletion:
    /// every record in the vault becomes unopenable in every backup, snapshot
    /// and replica that will ever be restored. See [`StatementKind::DropVault`].
    DefineVault {
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
        /// Whether the value is sealed before the record is encoded.
        ///
        /// Legal only on a vault, and refused everywhere else at execution
        /// rather than in the grammar — the parser does not know what kind of
        /// table a name refers to, and a refusal that depends on the catalog
        /// belongs where the catalog is. A secret field on an ordinary table
        /// would be sealed under a key nothing holds.
        ///
        /// There is no `ALTER FIELD … SECRET`, deliberately. Turning the marker
        /// on leaves every existing record in the clear and turning it off
        /// leaves every existing record unreadable; both are a table that is
        /// half one thing, and neither is a state a single statement should be
        /// able to produce.
        secret: bool,
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
        /// How far the user reaches, or the store when absent.
        scope: Option<ReachRef>,
        /// What the user may do.
        role: UserGrant,
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
    /// `ALTER NAMESPACE prod REPLICATION FACTOR 3`
    ///
    /// Turning replication on for a namespace that already holds data, and off
    /// again — both directions, because a policy you cannot withdraw is a
    /// policy you will hesitate to set (owner requirement D12).
    ///
    /// It carries only the replication, rather than a record of optional
    /// fields, for the reason [`UserChange`] gives: a struct of `Option`s makes
    /// *leave this alone* and *set this to nothing* the same shape, and the
    /// executor is then trusted to tell them apart. Here there is nothing else
    /// the statement can touch.
    ///
    /// There is no way to say *un-state it* — the clause moves between stated
    /// values and never back to never-stated, because never-stated is a fact
    /// about a namespace's history and not a setting.
    AlterNamespace {
        /// The namespace being changed.
        name: Name,
        /// What its replication becomes.
        replication: Replication,
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
        /// How many log records to keep, or nothing to leave retention alone.
        ///
        /// `RETAIN NONE` is the way back to unbounded, and it is spelled rather
        /// than implied by a zero: `RETAIN 0 RECORDS` would read as *keep
        /// nothing*, which is the one thing this must never be mistaken for —
        /// the log's last record is what lets a level follower be served at all.
        retain: Option<Option<u64>>,
    },
    /// `DEFINE FAILOVER AWARENESS 10s COLLECTION 10s ROUND 1s CAMPAIGN 1s LEASE 30s`
    ///
    /// The periods a cluster waits before it replaces a leader. Nameless, like
    /// [`Self::DefineNode`], because there is exactly one policy per cluster: a
    /// second row would be a second answer to a question that admits one, and
    /// nothing downstream would know which to read.
    ///
    /// **Every clause is required, and that is the difference from
    /// [`Self::DefineNode`].** That statement amends a row, so a clause left out
    /// means *leave that field alone*. This one replaces a SET whose members are
    /// checked against one another, so a partial statement could only either mix
    /// new values with old under a single version, or perform a
    /// read-modify-write the operator cannot see.
    ///
    /// The ordering pair is deliberately absent here. The leadership a policy was
    /// written under and which setting under it this was are supplied where the
    /// statement is executed, because an operator who could type either could
    /// write a policy that outranks a successor's — the exact ordering the epoch
    /// exists to prevent.
    DefineFailover {
        /// How often this node refreshes what it knows about its peers.
        awareness: Duration,
        /// How long a follower waits between collecting from its upstream.
        collection: Duration,
        /// How long one election round may take.
        round: Duration,
        /// How often a node that may write checks whether to stand.
        campaign: Duration,
        /// How long a granted leadership is held before it must be renewed.
        lease: Duration,
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
        /// Which node the row is about, when the declaration bound one.
        ///
        /// Already sixteen bytes rather than the text that was written: the
        /// spelling is checked where the span is, so a mistyped id is refused
        /// at the statement that wrote it instead of becoming a row that names
        /// a node nobody will ever be.
        node: Option<[u8; 16]>,
        /// How far that peer may collect this store's log, when it may at all.
        ///
        /// `None` is the declaration saying nothing, which is the refusal: a
        /// peer nobody subscribed receives no records. Spelled with the same
        /// [`ReachRef`] a grant is spelled with, because a subscription **is** a
        /// read grant over the addresses it names, and two spellings of one
        /// thing are two things that can come to disagree.
        replicates: Option<ReachRef>,
        /// The range that peer stands to lead, when the declaration placed one
        /// (`LEADS`).
        ///
        /// Never the store — every node that stands at all stands for the store
        /// already — so the parser takes `NAMESPACE`, `DATABASE` and `SHARD`
        /// only. `None` is the row as it has always been.
        leads: Option<ReachRef>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE KAFKA CONSUMER orders_in FROM 'broker:9092' TOPIC 'orders' …`
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
    /// are also reported by `INFO FOR KAFKA CONSUMER`, because a guarantee documented
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
    /// `DROP KAFKA CONSUMER orders_in` — stops it and forgets the declaration.
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
    /// `GRANT manage ON NAMESPACE prod TO ada`
    ///
    /// # A different thing from the grant above it, on purpose
    ///
    /// [`StatementKind::Grant`] narrows a user *within* the tenancy they were
    /// declared in, by naming a table. This one says what they may do and how
    /// far it goes, and the two do not compose into one statement because they
    /// answer different questions: one is "which of my tables", the other is
    /// "how much of this store".
    ///
    /// The reach is keyword-led in all three spellings, so a table can never be
    /// read as a reach — see [`ReachRef`].
    GrantAuthority {
        /// What is being given — one or more kinds, as written.
        kinds: Vec<Name>,
        /// How far it goes.
        reach: ReachRef,
        /// Who it is for.
        user: Name,
    },
    /// `REVOKE manage ON NAMESPACE prod FROM ada`
    ///
    /// # Taking away the last one is allowed here
    ///
    /// The opposite of [`StatementKind::Revoke`]'s rule, and for the reason
    /// that rule exists: a table grant going from one to none *widens* a user
    /// back to their role, so the last one is refused. An authority going from
    /// one to none leaves them holding nothing, which is the narrowest a user
    /// can be and cannot be a surprise.
    RevokeAuthority {
        /// What is being taken away.
        kinds: Vec<Name>,
        /// The reach it was at. Taking away `manage` at a namespace leaves
        /// `manage` at a database inside it exactly where it was: the model's
        /// only implication runs downward through *holding*, not through
        /// removal.
        reach: ReachRef,
        /// Who it was for.
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
    /// `DROP ANALYZER simple` — refused while a field still names it.
    ///
    /// A field attaches an analyzer by **name**, so nothing in the catalog
    /// enforces the link and a removal would leave a field pointing at a name
    /// that no longer resolves. The symptom of that is a search which quietly
    /// stops matching, which is why this refuses rather than cascades.
    DropAnalyzer {
        /// The analyzer to undeclare.
        name: Name,
    },
    /// `DROP REPLICA warsaw` — stops counting an endpoint as a peer.
    ///
    /// The peer is not told and its data is not chased. Replication here is
    /// declarative, so this statement says only that we no longer count that
    /// endpoint, and a peer that disagrees is an operator's question.
    DropReplica {
        /// The peer to undeclare.
        name: Name,
    },
    /// `DROP DATABASE staging` — refused while it still holds a table.
    ///
    /// The bound is inherited from `DELETE … LIMIT`: a destructive statement
    /// with no predicate at all is the widest one this language can be asked to
    /// run, so it refuses while anything is inside and counts what it found. A
    /// `CASCADE` word is deliberately absent — it is the unbounded form under
    /// another spelling.
    DropDatabase {
        /// The database to undefine.
        name: Name,
    },
    /// `DROP NAMESPACE acme` — refused while it still holds a database.
    DropNamespace {
        /// The namespace to undefine.
        name: Name,
    },
    /// `DROP GRAPH social` — refused while a table still belongs to it.
    ///
    /// Refuses rather than orphaning, on the same reasoning as `DROP DATABASE`:
    /// a membership left pointing at an id nothing resolves would surface later
    /// as a walk that finds no graph, rather than now as the drop that caused
    /// it.
    DropGraph {
        /// The graph to undefine.
        name: Name,
    },
    /// `DROP EDGE works_at` — the kind and every adjacency entry it wrote.
    ///
    /// The entries go with it, in the same transaction. A kind whose definition
    /// was removed while its adjacency stayed would leave every one of those
    /// entries pointing at an id nothing resolves, and a walk would reach through
    /// a join that no longer exists.
    DropEdge {
        /// The edge kind to undefine.
        name: Name,
    },
    /// `DROP VECTOR embeddings` — the store, its records and its index.
    ///
    /// The same act `DROP TABLE` performs, reached by the word that created the
    /// thing. Two spellings for one effect is what the round trip already
    /// requires: `INFO` reports a vector store as `DEFINE VECTOR`, so a reader
    /// who has only ever seen that word must have a way to undo it without
    /// having to learn that it was a table underneath.
    DropVector {
        /// The store to undefine.
        name: Name,
    },
    /// `DROP GEO places` — the store, its records and its index.
    ///
    /// Exists for the reason [`StatementKind::DropVector`] does: `INFO` reports
    /// a geo store as `DEFINE GEO`, so a reader who has only ever seen that word
    /// must have a way to undo it without first having to learn that it was a
    /// table underneath.
    DropGeo {
        /// The store to undefine.
        name: Name,
    },
    /// `DROP VAULT team` — the vault, its records, and the key that opened them.
    ///
    /// The one drop in this language that is a **crypto-shred** rather than a
    /// delete, and the distinction is the whole reason the per-vault key level
    /// exists. Deleting the rows is a statement about the live table; destroying
    /// the wrapped key is a statement about the data, because every copy of
    /// those records in every backup, snapshot and replica that will ever be
    /// restored is ciphertext under a key that no longer exists anywhere.
    ///
    /// It is therefore not undoable by restoring a backup, which is exactly what
    /// makes the claim worth making and exactly what makes it dangerous.
    DropVault {
        /// The vault to undefine.
        name: Name,
    },
    /// `DEFINE QUEUE jobs TIMEOUT 30s ATTEMPTS 5 SCHEMAFULL IN work`
    ///
    /// The eighth word in the row, and the first one whose whole capability is
    /// a **hold that lapses**. Written out as what it stands for, a queue is an
    /// ordinary table plus two rules the store enforces and a caller cannot:
    /// a claim writes a deadline, and a record whose deadline has passed is
    /// claimable again.
    ///
    /// Unlike [`StatementKind::DefineVector`] and [`StatementKind::DefineGeo`]
    /// it does **not** desugar into fields and an index, because there is
    /// nothing to declare that would produce the behaviour — a `claimed_until`
    /// field on a plain table is a field, not a hold. What makes it a queue is
    /// the kind, which is why the kind is what is stored.
    DefineQueue {
        /// The name to create.
        name: Name,
        /// How long a claim holds a record before it lapses.
        ///
        /// Required, with no default, for the reason `DEFINE VECTOR`'s width is:
        /// declaring it is the whole capability. A queue whose holds never lapse
        /// is a table with two extra fields, and a timeout the store guessed
        /// would hand work to a second worker at a moment nobody chose.
        timeout: Duration,
        /// How many times one record may be handed out, when a ceiling was named.
        ///
        /// Absent is unlimited, which is a legitimate choice for work that
        /// cannot poison — and a visible one, because leaving the clause out is
        /// what says it.
        attempts: Option<u32>,
        /// Whether a record carrying a field nobody declared is refused.
        ///
        /// The same flag [`StatementKind::DefineTable`] carries, and it is here
        /// because the first consumer to reach for a queue needed it. A queue
        /// declares no columns, so it is **lenient by default** — the rule a
        /// declared table already keeps, read properly: strictness constrains
        /// declared fields, and a word with no field list has nothing to
        /// constrain until `DEFINE FIELD` arrives afterwards.
        schemafull: bool,
        /// The graph this queue belongs to, when it belongs to one.
        ///
        /// A queue is an ordinary table plus a hold, and there was never a
        /// reason it could not be an end of a link. Before this clause a queue
        /// could not: `DEFINE EDGE` refuses a table that belongs to no graph,
        /// and declaring the table first and the queue second is refused
        /// because the name is taken — so a table that had to be both was
        /// simply unrepresentable.
        graph: Option<Name>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE SERIES readings RETAIN 30d`
    ///
    /// A table whose answer has a floor. Past the retention a record is not
    /// returned, whether or not its bytes have been removed — the removal is a
    /// separate act, so a pass that lags costs storage and never an answer.
    ///
    /// Like [`StatementKind::DefineQueue`] it does not desugar into fields and
    /// an index, because there is nothing to declare that would produce the
    /// behaviour: a `retain` field on a plain table is a field, not a floor.
    /// What makes it a series is the kind, which is why the kind is what is
    /// stored — and the kind also fixes the identity, because the floor is a
    /// position in the key and only a time-carrying identity has one.
    DefineSeries {
        /// The name to create.
        name: Name,
        /// How far back the table answers.
        ///
        /// Required, with no default, for the reason `DEFINE QUEUE`'s timeout
        /// is: declaring it is the whole capability, and a retention the store
        /// guessed would drop records at a boundary nobody chose.
        retain: Duration,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP SERIES readings`
    ///
    /// Removes the table and everything in it, including the records past the
    /// floor that reads had stopped answering with. They are records the store
    /// still held; what the floor governed was the answer, not the storage.
    DropSeries {
        /// The name to remove.
        name: Name,
    },
    /// `DROP QUEUE jobs`
    ///
    /// Removes the table and everything in it, held records included. A hold is
    /// a field on a record rather than a resource somebody else owns, so there
    /// is nothing here to wait for and nothing to release first.
    DropQueue {
        /// The queue to undefine.
        name: Name,
    },
    /// `DEFINE VIEW active AS SELECT * FROM users WHERE active = true`
    ///
    /// A name for a read. Nothing is stored under it and nothing is maintained:
    /// a statement naming the view is rewritten to carry the read, and the read
    /// runs the way any other materialised read runs.
    ///
    /// # The read is kept as text, not as a tree
    ///
    /// The same choice a field's `DEFAULT` makes, and for the same stated
    /// reason: a definition keeps the text it was written as, so `INFO` answers
    /// with the statement somebody typed rather than with a re-rendered
    /// statement that happens to mean the same thing. It also keeps the stored
    /// record stable across a grammar that grows — a serialised syntax tree
    /// would have to be versioned every time [`Select`] gained a field, and a
    /// view written before a clause existed would decode into a read that had
    /// silently lost it.
    ///
    /// The text is **parsed here** all the same, so a view that is not one
    /// `SELECT` is refused where it is written rather than on the first read.
    DefineView {
        /// The name to create.
        name: Name,
        /// The read, exactly as it was written.
        read: String,
        /// Whether re-defining an existing name is accepted.
        ///
        /// It accepts rather than replaces: a repeat definition is refused by
        /// the catalog's own name reservation, so changing a view is `DROP VIEW`
        /// and then `DEFINE VIEW`, and this clause only makes a provisioning
        /// script re-runnable.
        if_not_exists: bool,
    },
    /// `DROP VIEW active`
    ///
    /// Removes the definition, which is all there is: a view holds no records,
    /// no index and no keyspace, so nothing survives it and nothing else has to
    /// be cleaned up.
    DropView {
        /// The view to undefine.
        name: Name,
    },
    /// `CLAIM FROM jobs` · `CLAIM 10 FROM jobs`
    ///
    /// Takes the first claimable records in identity order and holds each of
    /// them until the queue's declared timeout has passed, answering with the
    /// records so the worker can do the work.
    ///
    /// **It is a statement rather than a clause on `SELECT`**, because it
    /// writes, and a reading verb that wrote would be lying about what it does —
    /// the same reason `RELATE` is its own statement rather than a flavour of
    /// `CREATE`.
    ///
    /// **Nothing claimable answers zero records and is not an error.** A worker
    /// polls; an empty queue is the ordinary case, not a fault.
    ///
    /// **It is not idempotent**, and that is worth knowing before relying on it:
    /// a worker whose reply is lost and which asks again receives a *different*
    /// record, while the first stays held until its deadline passes. Nothing is
    /// lost — delivery is at-least-once — but a claim retry is not free.
    Claim {
        /// The queue to take from.
        table: TableRef,
        /// How many records at most.
        ///
        /// One when the statement named no number.
        ///
        /// Zero is refused by the grammar, because a claim for no records is not
        /// a claim. The **upper** bound is the store's refusal rather than the
        /// grammar's, on the split `DEFINE VECTOR` already makes about its
        /// distance: how much a store will hand out in one statement is the
        /// store's question, and unbounded it is one statement holding the whole
        /// queue for the whole timeout while every other worker waits.
        count: u64,
        /// Where the statement sits.
        span: Span,
    },
    /// `CLAIM jobs:7`
    ///
    /// A hold on the record the caller names, rather than on whichever record
    /// the walk reaches first.
    ///
    /// **The same write by a second door.** It sets the two per-record fields
    /// [`Self::Claim`] sets, under the same declared timeout, so replication,
    /// restart and leader change are unchanged — a claim is still an ordinary
    /// logged write.
    ///
    /// **Existence is an error and contention is an answer.** A record that is
    /// not there raises, as `RELEASE` does, because a caller who named a record
    /// has to be told it named nothing. A record somebody holds, or one whose
    /// attempts are spent, answers **no records and no error**, which is the
    /// selecting form's own convention: nothing claimable is the ordinary case.
    ///
    /// **It skips the walk, so arrival order is a promise of [`Self::Claim`]
    /// alone.** A caller mixing the two forms can take a record the walk had not
    /// reached, which is the point of naming one.
    ///
    /// **The attempt count moves.** A hand-out is a hand-out however the record
    /// was chosen — so a queue used as a lock table is declared without an
    /// `ATTEMPTS` ceiling, or it stops locking once the ceiling is reached.
    ClaimRecord {
        /// The record to hold.
        target: RecordTarget,
        /// Where the statement sits.
        span: Span,
    },
    /// `RELEASE jobs:7` · `RELEASE jobs:7 FOR CONSUMER 'billing'`
    ///
    /// Clears a hold now rather than at its deadline, so a worker that knows it
    /// has failed — or that is shutting down — returns its work in milliseconds
    /// instead of in `TIMEOUT`.
    ///
    /// It does **not** touch the attempt count. The count is taken at the claim,
    /// and a record that was handed out was handed out whatever happened next;
    /// moving it here would make a deliberate hand-back and a crash count
    /// differently for no reason a caller could predict.
    ///
    /// **The bare form compares instances and the named form compares groups**,
    /// exactly as [`Self::ReleaseAll`] does, and for the same reason: taking a
    /// live colleague's work is the operation you have to type out. The named
    /// form exists because a client that holds ONE connection for many logical
    /// callers is minted a fresh instance at every `USE CONSUMER`, so the
    /// instance-strict form cannot say *let go of the record this caller took*.
    ///
    /// **Naming a consumer is not a master key.** A record another group holds
    /// gives the same refusal the bare form gives, carrying the holder's name,
    /// and a hold nobody signed belongs to no group and is freed by the bare
    /// form alone.
    Release {
        /// The record to release.
        target: RecordTarget,
        /// The group whose hold to clear, when the statement named one.
        consumer: Option<String>,
        /// Where the statement sits.
        span: Span,
    },
    /// `RELEASE ALL FROM jobs` · `RELEASE ALL FROM jobs FOR CONSUMER 'billing'`
    ///
    /// Clears every hold this session's **instance** holds in one queue, or
    /// every hold a named **consumer** holds there.
    ///
    /// **It names its queue, for the reason [`Self::Claim`] names its queue.**
    /// A form that named none would have to sweep every queue in the database:
    /// unbounded in cost, and — worse — a caller granted write on some of them
    /// would get a partial success that looked like a whole one, because the
    /// statement cannot refuse a table it was never told about. One table is one
    /// permission question with one answer.
    ///
    /// **The bare form is the safe one and the group form is spelled out.** A
    /// worker that crashed and came back holds a *new* instance, so reclaiming
    /// its predecessor's work is the named-consumer form — the operation that
    /// can take a live colleague's work is the one you have to type.
    ///
    /// **It answers the records it released, not a count.** A caller cannot list
    /// what it holds without reading first, and a session ending after a crash
    /// is the caller least able to read anything; a number would leave that read
    /// where it is.
    ///
    /// The attempt count is untouched, for [`Self::Release`]'s reason.
    ReleaseAll {
        /// The queue to let go of.
        table: TableRef,
        /// Whose holds to drop, or this session's instance when absent.
        consumer: Option<String>,
        /// Where the statement sits.
        span: Span,
    },
    /// `ALTER TABLE users ALTER FIELD email TYPE string REQUIRED`
    ///
    /// Redeclares a field that already exists, which a second `DEFINE FIELD`
    /// cannot do — the catalog reserves the name, so the second one is refused
    /// as taken. The declaration is replaced whole rather than patched: a
    /// statement that changed only the parts it mentioned would make *leave the
    /// default alone* and *remove the default* the same sentence, which is the
    /// reason [`UserChange`] is an enum rather than a record of options.
    ///
    /// The drop and the declaration land in one commit, so the rows are held to
    /// the **new** declaration by the store's own schema pass — an alteration no
    /// stored row satisfies is refused, writing nothing at all.
    AlterField {
        /// The field's name.
        name: Name,
        /// The table it is declared on.
        table: TableRef,
        /// What it may now hold.
        kind: FieldKind,
        /// Whether it must now be present.
        required: bool,
        /// What fills it when a write omits it, as written.
        default: Option<Written>,
        /// The analyzer its text is turned into terms by.
        analyzer: Option<Name>,
        /// What a value must satisfy.
        assert: Option<Assertion>,
    },
    /// `ALTER TABLE users SET SCHEMAFULL` · `… SET SCHEMALESS`
    ///
    /// The one thing about a table worth changing after it exists.
    ///
    /// **It changes the declaration and does not re-check the rows already
    /// stored.** A schema here is a rule about what may be *written*, so going
    /// schemafull binds every write from that commit onwards and leaves earlier
    /// records exactly as they are — which is also what keeps this statement
    /// bounded. Scanning the table would make a `DEFINE`-shaped statement do
    /// work proportional to the data, and this wave refuses that in
    /// `DROP DATABASE` for the same reason it declines it here (Q-233).
    AlterTable {
        /// The table to change.
        table: TableRef,
        /// What to change about it.
        change: TableChange,
    },
    /// `CHECK TABLE readings`
    ///
    /// Every stored record that disagrees with what the table declares **now**,
    /// as an answer rather than a refusal.
    ///
    /// A table that was strict from the start cannot hold such a record: the
    /// apply path saw every write. A table that *became* strict is held to its
    /// new declaration at the moment it becomes so, and never again. Between
    /// those two sits the table nobody has checked — one restored from a backup
    /// taken before a declaration, or one whose operator wants to know what
    /// stands in the way of making a field required before writing the statement
    /// that would refuse.
    ///
    /// It reads the whole table, which is the only honest way to answer, and
    /// that is why it is a statement somebody runs and not something the store
    /// decides to do.
    CheckTable {
        /// The table to hold to its own declarations.
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
    /// `CREATE users = { … }` — or `CREATE users:1 = { … }` when the caller has
    /// a name for the record already.
    Create {
        /// Where the record goes, and who named it.
        target: CreateTarget,
        /// Its whole content.
        value: Expr,
        /// What the statement answers with. `BEFORE` is refused: there was no
        /// record before, and a statement that answered `NONE` to a question
        /// somebody meant would be worse than one that says the question does
        /// not apply.
        answer: Answer,
    },
    /// `INSERT INTO users (name, email) VALUES ('ada', 'a@x'), ('grace', 'g@x')`
    ///
    /// # Why the identity is absent from the statement
    ///
    /// There is nowhere to write one. A caller who has an identity already —
    /// an import, a migration, a foreign key — writes `CREATE users:1 = { … }`,
    /// which is unchanged and stays the way to say that. This statement is for
    /// the other case, which is the common one: the caller has records and no
    /// names for them, and asking a human to invent a name per record is asking
    /// for the collision they will eventually write.
    ///
    /// # Why the columns are names and the values are values
    ///
    /// The column list is **grammar**. It is parsed as names, so a caller's text
    /// cannot arrive in that position and be read as one — the same property the
    /// query builder is built around, and the reason a supplied value binds
    /// after the script is parsed rather than being formatted into it.
    Insert {
        /// The table the records are written to.
        table: TableRef,
        /// The fields every row supplies, in the order they were written.
        columns: Vec<Name>,
        /// One row per record.
        ///
        /// Every row holds exactly as many values as there are columns, and
        /// that is checked **at parse**: a row of the wrong length is a
        /// statement the author mistyped, and finding out at the write means
        /// finding out after some of the batch is already decided.
        rows: Vec<Vec<Expr>>,
    },
    /// `SELECT * FROM …`
    ///
    /// Boxed, as [`StatementKind::Explain`] already boxes the same type. A read
    /// is much the widest statement this language has — clauses, projections, a
    /// source that may itself hold a join — and an enum is as wide as its widest
    /// variant, so unboxed it made every `COMMIT` and every `USE` in a parsed
    /// script cost what a `SELECT` costs.
    Select(Box<Select>),
    /// `REVEAL password FROM team:github` · `REVEAL * FROM team:github`
    ///
    /// The **only** statement that turns a sealed value back into a plaintext,
    /// and it exists as a separate verb rather than as a clause on `SELECT` for
    /// one reason: a read that could return a secret by accident eventually
    /// does. `SELECT` over a vault is refused outright and its message names
    /// this word, so the path to a plaintext is one a caller had to type.
    ///
    /// It names **one record** and takes no `WHERE`. That is not a simplification
    /// to be relaxed later — a filter over a secret is an oracle that answers one
    /// bit per statement, and an ordering is the same oracle more slowly. Both
    /// are refused, and a verb with no place to put them is the cheapest way to
    /// keep refusing them.
    ///
    /// Naming exactly one record is also what makes the audit row meaningful:
    /// *who opened what, and when* is a sentence only if *what* is a record.
    Reveal {
        /// The record to open.
        target: RecordTarget,
        /// The secret fields to open, or all of them when empty (`*`).
        ///
        /// A named field that is not secret is refused rather than returned in
        /// the clear: `REVEAL` answers with plaintext, and a caller reading its
        /// answer has no way to tell which entries were ever sealed.
        fields: Vec<Name>,
        /// Where the statement sits.
        span: Span,
    },
    /// `ADD RECIPIENT 'ops-escrow' TO team:github KEY $wrapped`
    ///
    /// A record's recipients are the parties that may one day open it, and this
    /// adds one. **The engine interprets neither half.** The name is text it
    /// stores and returns; the material is a value it stores and returns.
    /// Exactly one name means anything here — `#vault`, the store's own entry —
    /// and that name is refused, so nothing a caller writes can collide with it.
    ///
    /// The opacity is the feature rather than a shortcut. What a recipient's
    /// material *is* — a data key wrapped under somebody's public key, a handle
    /// into an application's own key service, a capability — is a question this
    /// store deliberately cannot answer, because answering it would mean holding
    /// the second key hierarchy that decides it. The application owns the
    /// sharing scheme; the record carries the set.
    ///
    /// It does **not** need an unsealed store. Nothing is unwrapped and nothing
    /// is decrypted, which matters most for its counterpart below: revocation is
    /// the one operation you least want to depend on an operator being present.
    AddRecipient {
        /// The record whose recipient set is added to.
        target: RecordTarget,
        /// The recipient's name. Text the store never reads.
        ///
        /// An expression rather than a literal, and the reason is the same one
        /// the material has: a name usually comes from somewhere — a directory,
        /// a form, another table — and a statement that could only take a
        /// literal would make every caller build one by formatting text into a
        /// script, which is the shape a query builder exists to avoid.
        recipient: Expr,
        /// The material stored under that name, unread.
        material: Expr,
        /// Where the statement sits.
        span: Span,
    },
    /// `REMOVE RECIPIENT 'ops-escrow' FROM team:github`
    ///
    /// The counterpart, and it **refuses a name that is not there** rather than
    /// reporting success. A revocation that silently matches nothing is the
    /// worst answer this statement could give: the operator reads `ok`, closes
    /// the ticket, and the recipient they meant to remove still holds whatever
    /// their entry gave them.
    RemoveRecipient {
        /// The record whose recipient set is removed from.
        target: RecordTarget,
        /// The recipient's name, bound like the one above.
        recipient: Expr,
        /// Where the statement sits.
        span: Span,
    },
    /// `UNSEAL VAULT WITH '…'` — the master key enters this process's memory.
    ///
    /// Store-wide, not per vault: the key it unwraps is the one every vault's
    /// own key is wrapped under. It affects **this process only** and survives
    /// no restart, which is the property that makes a restart safe and an
    /// unattended restart impossible — a real operational cost, written down
    /// rather than discovered.
    ///
    /// The passphrase arrives **in the statement** and never in an environment
    /// variable or an argument vector, both of which are readable by anything
    /// that can list a process. That is decision 3 of the design.
    UnsealVault {
        /// The passphrase, as written.
        passphrase: String,
        /// Where the statement sits, so a refusal can point at it without
        /// quoting what stands there.
        span: Span,
    },
    /// `SEAL VAULT` — and the master key leaves it.
    ///
    /// Takes no passphrase, because sealing is not an act that needs proving:
    /// the worst a caller can do by sealing is stop this process opening
    /// secrets, which is the safe direction and is undone by unsealing again.
    SealVault {
        /// Where the statement sits.
        span: Span,
    },
    /// `UPDATE users:1 = { … }` — the value is replaced, never merged.
    Update {
        /// The record to change.
        target: RecordTarget,
        /// How it changes.
        edit: Edit,
        /// What the record must already say for the change to happen.
        ///
        /// `UPDATE tasks:'t1' SET title = 'b' WHERE version = 1` — the language's
        /// compare-and-set. Evaluated against the record **as stored**, never
        /// against the payload the edit produces, so `WHERE version = 1` beside
        /// `SET version = 2` means what it reads as.
        ///
        /// A condition that does not hold is a **refusal**, and the failure
        /// discards the work above it in the transaction. That follows from what
        /// this verb already is: `UPDATE` asserts the record is present and
        /// refuses when it is not, so asserting it is also in a particular state
        /// is the same assertion one step further in. A count would make it the
        /// only assertion here a caller can ignore by forgetting to read a
        /// number, and forgetting costs two workers holding one job.
        ///
        /// [`StatementKind::Upsert`] deliberately has no such field: it asserts
        /// nothing about the record, so a condition on it would have to invent a
        /// meaning.
        condition: Option<Expr>,
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
    /// `DELETE person:1->works_at->company:1` — one edge, named by what it joins.
    ///
    /// The mirror of [`StatementKind::Relate`], and it exists because an edge's
    /// identity is **derived**: `RELATE` builds it from the two endpoints so that
    /// relating the same pair twice replaces rather than doubles, and never tells
    /// the caller what it built. Without this form, removing an edge would mean
    /// reconstructing a string the language has no statement that shows —
    /// a caller depending on an internal encoding to undo what one statement did.
    ///
    /// Separate from [`StatementKind::Delete`] for the reason the conditional
    /// form is separate: it names its subject differently, and one variant
    /// wearing three targets in an `Option` would put the difference in a field
    /// rather than in the grammar.
    DeleteEdge {
        /// The edge's source.
        from: RecordTarget,
        /// The edge kind, or the edge table, the relation was recorded in.
        edges: TableRef,
        /// The edge's target.
        to: RecordTarget,
        /// What the statement answers with. `AFTER` is refused, as it is for any
        /// delete.
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
    /// `DELETE FROM events:1000..2000 LIMIT ALL` — every record in a span of
    /// identities.
    ///
    /// The retention statement, once a table's identities are its time order.
    /// [`Self::DeleteWhere`] over the same records reads the table, tests each
    /// one and removes the matches; this walks the keyspace between two
    /// positions and removes what is there, so its cost is the size of what it
    /// removes rather than the size of what it keeps.
    ///
    /// # Why there is no condition
    ///
    /// A conditional delete re-tests every candidate against the whole
    /// condition, because an index **narrows** and the condition decides. A span
    /// narrows nothing — it *is* the set the statement named — so there is
    /// nothing left to decide and nothing to re-test. Allowing a `WHERE` beside
    /// it would put the two rules in one statement and make the answer depend on
    /// which of them the reader believed.
    ///
    /// The bound is required for [`Self::DeleteWhere`]'s reason: removing an
    /// unbounded set has to be said.
    DeleteSpan {
        /// The table being cleared out.
        table: TableRef,
        /// The first identity to remove, always included.
        lower: Identity,
        /// The last, included only when the bound was written `..=`.
        upper: Identity,
        /// Whether the upper bound is itself removed.
        inclusive: bool,
        /// Where the span sits, for a refusal about a bound.
        span: Span,
        /// How much this statement may remove.
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
    /// `VERIFY` — run every check a `COMMIT` runs, then discard the work.
    ///
    /// A third thing to do with an open transaction, and therefore a third word
    /// rather than a flag on one of the other two. `COMMIT` checks and keeps;
    /// `CANCEL` discards **without** checking, because every check that refuses
    /// a write runs inside the commit; this checks and discards.
    ///
    /// It exists because there was no way to ask *"would this be refused?"*
    /// other than to be refused, and being refused means having sent the write.
    Verify,
}

/// What an `INFO FOR` asks about.
///
/// Sixteen subjects, and each one has **exactly one** rule deciding what the
/// caller may see. That is why they are separate subjects rather than one with a
/// filter argument: a statement whose answer mixes two permission levels can only
/// give a partial answer or a confusing refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoSubject {
    /// `INFO FOR HISTORY OF orders:1` — what happened to one record, newest
    /// first.
    ///
    /// Distinct from [`Self::Versions`], which is a **conflict report**: it
    /// answers whether a record is contested right now, and on a single-leader
    /// range that is one version and nothing else. This answers what the record
    /// became, and when — a different question that the two were confused for
    /// until it was measured (Q-739).
    ///
    /// It reads the log rather than a second event store. The store has written
    /// one all along; what it lacked was a way to ask.
    History(RecordTarget),
    /// `INFO FOR STORE` — the namespaces.
    Store,
    /// `INFO FOR NAMESPACE` — the databases in the selected namespace.
    Namespace,
    /// `INFO FOR DATABASE` — the tables in the selected database.
    Database,
    /// `INFO FOR TABLE users` — one table's shape, fields and indexes.
    Table(TableRef),
    /// `INFO FOR GRAPH social` — the tables that belong to one graph.
    ///
    /// A graph with no members answers with an empty list rather than an error:
    /// a graph you have just declared exists, and reporting it as absent would
    /// make the first thing anyone does after declaring one look like a failure.
    Graph(Name),
    /// `INFO FOR VECTOR embeddings` — one vector store's width, distance and
    /// measured recall.
    ///
    /// Distinct from `INFO FOR TABLE`, which reports fields and indexes, because
    /// the question a vector store is asked is not *what is in it* but **how good
    /// is it**: recall is the number that says whether an approximate answer is
    /// worth having, and it is the one thing the table view can never carry,
    /// since it is a property of a measurement rather than of a declaration.
    ///
    /// It reports the recall that was **measured**, and the parameters it was
    /// measured at, or says it has never been measured. It never computes a
    /// plausible figure: an approximate index whose recall came from a formula is
    /// a number nobody checked.
    Vector(Name),
    /// `INFO FOR GEO places` — one geo store's field and index.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Vector`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a collection and the store stops being one.
    ///
    /// It carries no measurement, and that is not an omission. A vector index
    /// answers approximately, so what it is worth is a question only a
    /// measurement settles; a spatial index answers exactly, so there is nothing
    /// about it a number could report that the declaration does not already say.
    Geo(Name),
    /// `INFO FOR VAULT team` — one vault's fields, and which of them are sealed.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Geo`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a table and the store stops being one — and here that is
    /// not a cosmetic loss, because a table has no `SECRET` to re-declare.
    ///
    /// It reports **which fields are sealed and nothing about what they hold**.
    /// That is the line this subject has to hold: an `INFO` that answered with a
    /// length, a fingerprint or a key identifier would be a slower oracle rather
    /// than none, and a reader would have no way to tell it was one.
    Vault(Name),
    /// `INFO FOR BUCKET media` — one bucket's name and the largest file it takes.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Vault`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a table and the store stops being one.
    ///
    /// **It also exists to be asked before a listing.** A route that lists a
    /// bucket needs to know a name is one, and until this subject existed there
    /// was no statement to ask — so the HTTP listing answered `200` with an
    /// empty body for a plain table while the three routes that write, read and
    /// delete a file all refused it. A caller then concluded the bucket was
    /// empty rather than absent, which is a wrong answer wearing a right one's
    /// clothes.
    Bucket(Name),
    /// `INFO FOR RECIPIENTS OF team:github` — who may one day open this record.
    ///
    /// The read half of the recipient set, and the reason the set is worth
    /// carrying at all: a set nothing can enumerate is write-only, and an
    /// application cannot answer *who can open this* by adding to it.
    ///
    /// It reports the names **and** their material, because the material is the
    /// application's own ciphertext and withholding it would make the round trip
    /// F1 asks for impossible. The store's own `#vault` entry is not among them:
    /// it is not a recipient anybody added, and listing it would invite an
    /// attempt to remove the one entry that must never go.
    Recipients(RecordTarget),
    /// `INFO FOR VERSIONS OF person:1` — every surviving version of one record,
    /// the node that wrote each, and whether they are contested.
    ///
    /// Its own subject rather than fields on an ordinary read, for the reason
    /// [`InfoSubject::Recipients`] is one: a per-record fact that almost no
    /// record has does not belong as a column on every read in the product. On a
    /// single-leader range the answer is one version and `concurrent: false`,
    /// and it answers there deliberately — a report that refused outside
    /// multi-master would make *is this contested?* unanswerable exactly where
    /// an operator who has just changed a namespace's class most wants to ask.
    Versions(RecordTarget),
    /// `INFO FOR AUDIT` — every recorded vault read; `BY 'ada'` narrows to one
    /// actor.
    ///
    /// The forensic question in the language. `REVEAL` records every read, but
    /// until this the trail could only be read from Rust — so an operator
    /// holding a compromised credential could not ask *what did it open* with a
    /// statement, which is the one moment they most need to.
    ///
    /// # The filter does not make this two subjects
    ///
    /// It is [`Option`] rather than a second variant because the whole trail and
    /// one actor's slice of it are the same answer under the same rule: the
    /// permission is identical, the shape is identical, and narrowing discloses
    /// strictly less. The rule this enum opens with — one rule per subject —
    /// is what forbids a filter that spans permission levels, and this one
    /// spans none.
    ///
    /// # It is answered only to the node's administrator
    ///
    /// The trail is stored store-wide rather than per tenancy, because a read
    /// is recorded before anyone knows whose it was. So there is no tenancy to
    /// scope the answer by, and the honest demand is `govern` held over the
    /// store itself — strictly narrower than any tenancy grant, and the reason
    /// one namespace's administrator cannot read another's reads.
    Audit(Option<Name>),
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
    /// `INFO FOR ACCESS TO TABLE orders` — who can reach this table, and how.
    ///
    /// The other direction of [`InfoSubject::User`]. That one answers *what may
    /// this user reach*, starting from a person; this one starts from an object
    /// and answers *who reaches it* — and an operator holding an incident needs
    /// the second question far more often than the first, because the thing they
    /// have is the table that leaked.
    ///
    /// # It is answered by asking, not by reading
    ///
    /// The answer is **not** derived from grants and authorities a second time.
    /// For every user the caller administers, the store signs a throwaway session
    /// in as that user and puts a real statement to the ordinary authorization
    /// path — the same function every `SELECT` and every `DELETE` goes through.
    /// A report that re-derived reachability would be a second evaluator, and two
    /// evaluators of one rule disagree eventually; the one that disagrees
    /// silently here is the one an auditor was trusting.
    ///
    /// Refuses rather than filters, for [`InfoSubject::User`]'s reason: its
    /// content *is* the permission system, and a partial account of who may do
    /// what reads as the whole account.
    Access(TableRef),
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
    /// `INFO FOR KAFKA CONSUMER orders_in` — one consumer's declaration and its
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
    /// `INFO FOR KAFKA CONSUMERS` — every declared consumer, and whether it is running.
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
    /// Whether `WITHOUT SCAN GUARD` was written, lifting the planner's veto.
    ///
    /// The guard it lifts is a **policy** and not a measurement: an index is
    /// served when it can produce at most half the table, and half is a
    /// threshold this store chose rather than a number a cost model produced.
    /// `plan::worth_serving` counts with a bounded probe, so what can be wrong
    /// here is the threshold and never the count — which is why the clause is
    /// spelled as lifting a guard rather than as overriding an estimate.
    ///
    /// It lifts the veto and does **not** choose the path. An override naming an
    /// index would be a router, and a router owes answers to every question a
    /// router raises: what an inapplicable named index does to the predicate,
    /// how it composes with ranking, what `EXPLAIN` then reports. This is one
    /// flag reaching one function — the ranking still chooses, an inapplicable
    /// index still changes nothing, and the worst case of misuse is the
    /// behaviour that shipped before the guard existed.
    ///
    /// A separate clause rather than a word on `USING INDEX`, deliberately: that
    /// clause is an assertion about what the read did, and a modifier turning it
    /// into an instruction would be a pun a reader can miss. This one cannot be
    /// missed.
    ///
    /// **Its removal condition, recorded at birth** (Q-494): it exists because
    /// the threshold is a policy, and it is retired when the planner acquires a
    /// cost model or statistics that make the policy unnecessary. A hint with no
    /// recorded removal condition is scar tissue — it freezes plans against a
    /// planner that has since improved, and nobody dares remove it because
    /// nobody remembers why it is there.
    pub lift_scan_guard: bool,
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
    ///
    /// An `Option` and not a `bool` beside a separate budget field, because the
    /// budget is meaningless without the permission: an exact scan has nothing to
    /// spend. Kept as one value so *"an effort with no approximation"* is
    /// unrepresentable rather than merely unreachable — the same reasoning
    /// `TableKind` records, and for the same reason, since a `Select` is built by
    /// the query builder as well as by the parser.
    pub approximate: Option<Approximation>,
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
    /// The point in the store's history the read answers from.
    ///
    /// `None` is the ordinary case: the read answers from the committed tail.
    pub version: Option<Version>,
    /// How far behind the node answering this read is allowed to be.
    ///
    /// `None` is the ordinary case: a read has no tolerance because it is
    /// answered here, and a node's own answer is never stale relative to itself.
    pub staleness: Option<Staleness>,
    /// Which nodes this read admits as its answerer.
    ///
    /// `None` is the ordinary case and means what `ANSWERED BY ANY` means: any
    /// copy may answer. See [`AnsweredBy`] for why this is a second axis rather
    /// than a tighter [`Staleness`].
    pub answered_by: Option<AnsweredBy>,
    /// Where the statement sits in the source.
    pub span: Span,
}

/// How far behind the node answering a read is allowed to be.
///
/// **A candidate filter, never a marker.** It does not ask to be told that an
/// answer was stale; it says which nodes may answer at all. A marker nobody is
/// obliged to read is not a guarantee, which is why a read no node can satisfy
/// is refused rather than quietly promoted to the leader.
///
/// The bound is a literal duration and a parameter is not accepted in its place,
/// the rule `TIMEOUT` already keeps: a tolerance a bound value could set is a
/// tolerance a caller could widen, and this one is meant to be readable in the
/// statement that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staleness {
    /// The tolerance, as the statement wrote it.
    pub within: Duration,
    /// Where the clause sits, for the refusal to point at.
    pub span: Span,
}

/// Which nodes a read admits as its answerer.
///
/// # Why this is not a tighter staleness bound
///
/// A follower at zero lag is still not authoritative. Being level a moment ago
/// says nothing about a write committing right now, so a bound of `0s` does not
/// linearise — and `0s` is refused anyway, because it admits no node at all
/// including the one being asked. Answering a read that had to come from the
/// leader out of a freshness bound is a wrong answer that raises no error, which
/// is why the two are separate controls and not one knob.
///
/// The two **compose** and neither widens the other: a read may name both, and
/// it is answered only where both hold.
///
/// # Why the clause is not called `AUTHORITY`
///
/// Because that word is already spoken for, and by the part of the language a
/// mistake would be most expensive in: [`ReachRef`] documents itself as *how far
/// an authority goes*, `DEFINE USER … ON prod.orders` writes one, and the whole
/// grant surface is built on the noun. A read clause wearing the same word would
/// make `AUTHORITY LEADER` look like a permission to every reader who met the
/// grant vocabulary first.
///
/// `ANSWERED BY` says the thing itself — who answers — and collides with
/// nothing. *Strong* and *consistency* were rejected as terms of art: both
/// promise a linearisability this engine does not claim, and the limit below is
/// exactly why.
///
/// # Why `LEADER`
///
/// Leadership is already this product's own vocabulary rather than an
/// implementation detail leaking out: it is a catalog row, `DEFINE REPLICA …
/// ROLES writable` declares it, and `INFO FOR NODE` reports it. A new word would
/// be a second name for something the reader already has one for.
///
/// # The limit that ships with it
///
/// Answered by the leader is not **repeatable**. Two such reads with writes in
/// between legitimately differ, and neither is wrong. The clause says where the
/// answer comes from; it does not hold the store still while the caller reads —
/// that is what a transaction is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnsweredBy {
    /// Which nodes the read admits.
    pub admits: Admitted,
    /// Where the clause sits, for a refusal to point at.
    pub span: Span,
}

/// The two answers `ANSWERED BY` takes.
///
/// Two and not three: a third waits for something that needs
/// it. Named values rather than a `bool`, for the reason every tag in this
/// workspace is named — a `bool` makes an unrecognised spelling silently become
/// one of the two, and here the silent direction is the unsafe one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Admitted {
    /// Any copy may answer. The default, and writable on purpose: a read that
    /// deliberately does not need the leader should be able to say so.
    #[default]
    AnyCopy,
    /// Only the node that decides writes for these records may answer.
    Leader,
}

/// A point in the store's history a read answers from.
///
/// # Why this is a sequence and not a timestamp
///
/// Records are versioned by a suffix on their own key, and that suffix is the
/// log sequence the version was written at. The sequence is the store's only
/// ordering authority: no log record carries a wall clock, and two commits
/// within the same millisecond are ordered by sequence and by nothing else.
///
/// So a timestamp could not name a point *between* those two commits — it would
/// be a spelling that looks more precise than the thing it addresses. The clause
/// names the number the store actually orders by, which is the same number an
/// answer reports and a caller can hand straight back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// The sequence to read at.
    pub at: u64,
    /// Where the clause sits, for a refusal to point at.
    pub span: Span,
}

/// What a caller accepted when they wrote `APPROXIMATE`, and what they will
/// spend on it.
///
/// The budget is the walk's speed-against-recall dial: how many candidates it
/// keeps in hand before it stops. Larger explores more and costs more, and the
/// trade belongs to **this read** rather than to the declaration — a caller who
/// needs a better answer for one query should not have to redeclare the store,
/// and one who needs a cheaper answer should not degrade everybody else's.
///
/// It never reaches the index **build**. The build walks the same graph to choose
/// a new node's neighbours, so a read's budget leaking into it would let two
/// replicas replaying one log with different reads interleaved build different
/// graphs — the determinism this index gave up its hierarchical layer to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approximation {
    /// `APPROXIMATE` — the walk spends the budget the engine was built with.
    Default,
    /// `APPROXIMATE EFFORT 200` — the walk keeps this many candidates.
    ///
    /// At least one, refused below that where it is written, as `DEPTH n` and
    /// `vector<n>` are: a walk that may keep no candidates is a search with no
    /// way to answer.
    Effort(usize),
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
    /// `SELECT * FROM events:1000..2000` — every record whose identity falls in
    /// a span.
    ///
    /// # Why this is a source and not a condition
    ///
    /// `WHERE id >= 1000 AND id < 2000` asks the same question and is answered
    /// by reading the table and testing every record. This is answered by
    /// **walking the keyspace between two positions**, because a record's key is
    /// its table prefix followed by its identity — so the records outside the
    /// span are not read, not decoded and not tested. The difference is the
    /// whole reason the variant exists, and it is a difference in cost of the
    /// same order as an index.
    ///
    /// # What it is a window over
    ///
    /// Identity order, which for both identity kinds this store issues is also
    /// **write order**: `Int` is a per-table counter, and `Uuid` is UUID v7,
    /// which carries a timestamp in its leading bits. So a span of identities is
    /// a span of time *as the store saw it*. It is not a span of an event time a
    /// record carries in a field — if events arrive out of order, those are two
    /// different questions, and the one this answers is the arrival.
    Range {
        /// The table.
        table: TableRef,
        /// The first identity in the span, which is always inside it.
        lower: Identity,
        /// The last, inside the span only when the bound was written `..=`.
        upper: Identity,
        /// Whether the upper bound is itself included.
        inclusive: bool,
        /// Where the whole source sits.
        span: Span,
    },
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
        /// `DEPTH n` — how many times the single hop repeats.
        ///
        /// `None` is a walk written out step by step, which is bounded because
        /// the steps are written. `Some(n)` is the only construct in the
        /// language that repeats, and `n` is an integer **literal** for that
        /// reason: a walk whose length comes from a parameter is a walk whose
        /// length is not in the statement, and nothing reading the statement
        /// could tell how far it goes.
        ///
        /// Answers with every distinct record reachable in `1..=n` hops. The
        /// start is marked seen before the first round, so a cycle terminates
        /// and no record is answered twice — which is what makes `n` bound the
        /// *work* and not merely the number written down.
        depth: Option<u64>,
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
    /// `variance(<expr>)` — the **sample** variance, dividing by `n − 1`.
    ///
    /// Sample rather than population because a table's rows are usually a
    /// sample of something, which is why the SQL standard's bare `VARIANCE` is
    /// `VAR_SAMP` and why Postgres spells it the same way. The population form
    /// is not a second name because the language can already say it:
    /// `variance(x) * (count(x) - 1) / count(x)`.
    ///
    /// Over fewer than two numbers, `NONE` — `n − 1` is zero there, and the
    /// spread of one value is not zero, it is unasked.
    Variance,
    /// `stddev(<expr>)` — the square root of [`Self::Variance`], and sample for
    /// the same reason.
    Stddev,
    /// `median(<expr>)` — the middle number, or the mean of the two middles.
    ///
    /// Numeric like `mean`, and refusing anything else for the same reason. An
    /// even count answers the mean of the two middles, which is a value that
    /// was never in the data — acceptable only because the fold is numeric; the
    /// same rule over a `datetime` or a `uuid` column would have to construct a
    /// value of a kind that has no arithmetic.
    Median,
    /// `collect(<expr>)` — every present value, in the order the records arrived.
    ///
    /// Over nothing, `[]` and not `NONE`, by `sum`'s rule: an answer every
    /// caller has to write `?? []` after is the wrong answer.
    Collect,
}

/// How much a fold holds while its group is still arriving.
///
/// The question exists because two answers to it are not interchangeable, and
/// the difference is invisible in the fold's *signature*: every fold takes many
/// values and answers one. What separates them is whether the one answer can be
/// computed as the values go past.
///
/// It is a property of the fold and not a rule about aggregation, for the same
/// reason [`crate::Purity`] is a property of the function: a single rule would
/// get one of them wrong in silence. `count`, `sum`, `mean`, `min`, `max`,
/// `variance` and `stddev` all reduce one value at a time — Welford's algorithm
/// carries `(count, mean, M2)` and is three numbers however long the group is.
/// `collect` and `median` cannot: `collect`'s answer **is** the collection, and
/// an exact median has to see every value before it knows which one is the
/// middle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// The answer is reducible one value at a time, in space that does not grow.
    Constant,
    /// The answer is a function of the whole group, so the whole group is held.
    WholeGroup,
}

impl Aggregate {
    /// Every fold, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::Count,
        Self::Sum,
        Self::Mean,
        Self::Min,
        Self::Max,
        Self::Variance,
        Self::Stddev,
        Self::Median,
        Self::Collect,
    ];

    /// How the fold is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Min => "min",
            Self::Max => "max",
            Self::Variance => "variance",
            Self::Stddev => "stddev",
            Self::Median => "median",
            Self::Collect => "collect",
        }
    }

    /// What this fold holds while its group arrives.
    ///
    /// Read by the executor's memory ceiling, which used to exempt every folding
    /// read by name on the grounds that *"its answer does not grow with the
    /// table"*. That was true of every fold the language had; it is false of
    /// `collect`, whose answer is the table (Q-227).
    #[must_use]
    pub const fn retention(self) -> Retention {
        match self {
            Self::Count
            | Self::Sum
            | Self::Mean
            | Self::Min
            | Self::Max
            | Self::Variance
            | Self::Stddev => Retention::Constant,
            Self::Median | Self::Collect => Retention::WholeGroup,
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

/// One field declared inside a table's parentheses.
///
/// Every field of [`DefineField`](StatementKind::DefineField) except the table,
/// which the surrounding statement names, and `if_not_exists`, which the
/// surrounding statement holds for the whole declaration. The two spellings are
/// therefore the same declaration written two ways, and the executor desugars
/// this one into the other rather than reimplementing what a field means —
/// which is what keeps a constraint declared here checking the rows already
/// there, exactly as the long spelling does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDeclaration {
    /// The field's name, unique within its table.
    pub name: Name,
    /// What the field is allowed to hold.
    ///
    /// Positional rather than introduced by `TYPE`: nothing but a type can
    /// stand after a column name, and the word would be noise in a list whose
    /// whole purpose is to be read down a page.
    pub kind: FieldKind,
    /// Whether the field must hold a value: present, and not `null`.
    pub required: bool,
    /// What a write supplying no value uses instead.
    pub default: Option<Written>,
    /// The analyzer this field's text becomes terms by, when it has one.
    pub analyzer: Option<Name>,
    /// What the value must satisfy, beyond its type.
    pub assert: Option<Assertion>,
}

/// The order an edge table holds a node's edges in: `ORDER BY at DESC`.
///
/// A single field name and a direction, and deliberately not an [`Ordering`],
/// which carries an expression because a `SELECT` sorts an answer it already
/// has. This one is not a sort at all — it becomes the **suffix of the endpoint
/// index's key**, so the edges arrive in this order because that is where they
/// are written, and a bounded read of the first few is an adjacent-key read
/// rather than a scan that throws most of its work away.
///
/// That is also why it is a field and not an expression: a key suffix has to be
/// derivable from the record by the writer, at write time, identically on every
/// replica. An expression would have to be evaluated to place a row, and any
/// change to it would silently mean the stored keys no longer match the
/// declaration they were written under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeOrdering {
    /// The edge property the order reads.
    pub field: Name,
    /// Whether the newest, or largest, comes first.
    pub descending: bool,
}

/// What `EDGE` said, on the statement that declared the table.
///
/// Three states rather than a `bool` beside an `Option<pair>`, for the reason
/// the catalog's table kind replaced three flags: the pair only means anything
/// on an edge table, and a field beside a flag would make "declares a pair but
/// is not an edge table" a thing a parser could hand downstream. Absent —
/// `None` on the statement — is a table that holds records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeClause {
    /// `EDGE`: a link between any two records is accepted.
    ///
    /// The permissive spelling stays, because a store that discovers its shape
    /// as it goes still has a word for that, and because every edge table
    /// declared before the clause existed is one of these (Q-297).
    Any,
    /// `EDGE FROM users TO users ORDER BY at DESC`: a link whose endpoints the
    /// table does not declare is refused.
    ///
    /// That refusal is the whole of what the clause buys — the difference in
    /// what a caller may **do** that earns it a place in the grammar.
    ///
    /// Boxed because the pair is several times the size of the other variant and
    /// this enum is carried by every `DEFINE TABLE`, edge table or not.
    Between(Box<EdgeEndpoints>),
}

/// The pair an `EDGE FROM … TO …` declared, and the order it holds them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeEndpoints {
    /// The table an edge leads out of.
    pub from: TableRef,
    /// The table an edge leads into.
    pub to: TableRef,
    /// The order a node's edges are held in, when the statement gives one.
    pub order: Option<EdgeOrdering>,
}

/// The one thing an [`AlterTable`](StatementKind::AlterTable) statement changes.
///
/// One variant rather than a bool, for the reason [`UserChange`] is an enum:
/// `SET SCHEMAFULL` and `SET SCHEMALESS` are two statements a reader writes,
/// and a `schemafull: bool` field would make a third shape — *change nothing* —
/// expressible in a statement that exists only to change something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableChange {
    /// `SET SCHEMAFULL` — from the commit onwards the declared fields are the
    /// whole story. Records written before it are not revisited.
    Schemafull,
    /// `SET SCHEMALESS` — a record may carry a field nobody declared.
    ///
    /// Refused on a **vault** and nowhere else. Everywhere else it only widens
    /// what is admissible, so no stored row can contradict it; on a vault the
    /// widening is the hole, because the marker that seals a field is `SECRET`
    /// on its declaration and a field nobody declared carries no marker.
    Schemaless,
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

/// What a `DEFINE USER` says the user may do.
///
/// Two spellings of one thing: a role is a *name for a set*, and the set is
/// what the store keeps. Both are kept because dropping the role would make
/// every existing statement and every existing record wrong to gain nothing —
/// three names cover the common cases, and the set covers the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserGrant {
    /// `ROLE editor` — the bundle that name stands for.
    Role(Name),
    /// `AUTHORITIES manage, read` — the set, said directly.
    ///
    /// This is what makes the rule a role could not express sayable: a holder
    /// of `manage` at a namespace who holds neither `read` nor `write` there.
    Authorities(Vec<Name>),
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
mod tests {
    use super::{Aggregate, Retention};

    /// The whole membership of [`Retention::WholeGroup`], asserted as a set.
    ///
    /// The same guard `Purity`'s membership tests give, for the same reason and
    /// against a sharper failure. `retention` forces a new fold to be
    /// *classified*, but the classification is a claim about what the fold costs
    /// and the two can be written apart. Both directions are wrong and only one
    /// is loud: a constant-space fold listed here is merely refused a memory
    /// exemption it deserved, while a whole-group fold left out keeps the
    /// exemption that says *"its answer does not grow with the table"* — and
    /// then `SELECT collect(x) FROM huge` is exactly the unbounded read the
    /// ceiling exists to refuse, waved through by name (Q-227).
    ///
    /// Adding a member is therefore allowed and cheap; the test exists so that
    /// **failing to** add one cannot happen quietly.
    #[test]
    fn the_folds_that_hold_their_whole_group_are_exactly_the_two_that_must() {
        let holding: Vec<&str> = Aggregate::ALL
            .iter()
            .filter(|fold| fold.retention() == Retention::WholeGroup)
            .map(|fold| fold.spelling())
            .collect();
        assert_eq!(holding, ["median", "collect"]);
    }

    /// Every fold is in `ALL`, and every spelling parses back to itself.
    ///
    /// `ALL` is what the parser reads to recognise a fold at all, so a variant
    /// missing from it is a fold nobody can write — and no other test would
    /// notice, because the grammar simply treats the word as a field name.
    #[test]
    fn every_fold_is_listed_and_every_spelling_names_it_back() {
        for fold in Aggregate::ALL {
            assert_eq!(
                Aggregate::parse(fold.spelling()),
                Some(*fold),
                "{} did not parse back to itself",
                fold.spelling()
            );
        }
        let spellings: Vec<&str> = Aggregate::ALL.iter().map(|fold| fold.spelling()).collect();
        let mut sorted = spellings.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            spellings.len(),
            "two folds share a spelling: {spellings:?}"
        );
    }
}
