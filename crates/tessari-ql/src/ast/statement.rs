//! Every statement the language has, as one enum.

use super::{
    Answer, ColumnDeclaration, ConsumerSource, CreateTarget, Credential, DeleteBound, EdgeClause,
    Edit, Expr, FieldMapping, FieldPath, GroupClauses, Identity, InfoSubject, Name,
    NamespaceChange, OnFailure, RangeExpr, ReachRef, RecordTarget, Select, SetCondition,
    SpaceBound, TableChange, TableExpiry, TableRef, TopicClauses, UserChange, UserGrant,
    WriteExpiry, Written,
};
use crate::token::Span;
use tessari_types::{
    Acknowledgement, Assertion, ConflictPolicy, Duration, FieldKind, Filter, IdentityKind, Number,
    RecordId, Replication, ReplicationClass,
};

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
        /// How many copies must hold a write here before it is acknowledged,
        /// and whether a request may ask for fewer (ADR-0106 D2) — `None` when
        /// the statement said nothing, which is not a stated level.
        acknowledge: Option<Acknowledgement>,
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
        /// The field every record's identity begins with: `DEFINE TABLE
        /// customers (region string, …) IDENTITY uuid PARTITION BY region`
        /// (ADR-0096). A record of it is named `'<region>:<uuid>'`.
        partition: Option<Name>,
        /// Whether a generated identity begins with a bucket of two hex digits
        /// so new records spread over the table's shards: `IDENTITY uuid
        /// SPREAD` (ADR-0113 D1).
        spread: bool,
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
        /// Whether the table's records expire: `DEFINE TABLE message (…) EXPIRE
        /// AFTER 7d` (ADR-0122 A1). `None` is a table that said nothing, and
        /// behaves exactly as every table did before the clause existed.
        expire: Option<TableExpiry>,
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
        /// `MAX n [EVICT NONE]` — the most keys the space holds (G036).
        limit: Option<SpaceBound>,
    },
    /// `DEFINE TOPIC events RETAIN 7d MAX BYTES 4096 PUBLIC RATE 100 PER 1m` —
    /// an append-only order of messages (G037).
    DefineTopic {
        /// The name to create.
        name: Name,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
        /// What the declaration says about keeping, sizing and opening it.
        clauses: TopicClauses,
    },
    /// `READ FROM events FOR CONSUMER 'billing' AFTER 41 LIMIT 100` — the
    /// messages of a topic after a position (G037).
    ReadTopic {
        /// The topic.
        topic: TableRef,
        /// The reader whose stored position the read starts after and advances.
        consumer: Option<String>,
        /// The position to read after, instead of the stored one.
        after: Option<Expr>,
        /// The most messages to answer.
        limit: Option<Expr>,
    },
    /// `DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s DELIVERIES 5
    /// IN FLIGHT 10 DEAD LETTER TO events_dead` — readers under one name who
    /// share a topic's messages and acknowledge each one (G042, ADR-0086).
    DefineGroup {
        /// The name its readers read under, as `FOR CONSUMER` spells it.
        name: String,
        /// The topic it reads.
        topic: TableRef,
        /// Whether re-defining an existing group is accepted.
        if_not_exists: bool,
        /// Its deadline, deliveries, width and dead letter.
        clauses: GroupClauses,
    },
    /// `DROP GROUP 'billing' ON TOPIC events` — the group and everything it
    /// holds in flight are forgotten.
    DropGroup {
        /// The group.
        name: String,
        /// Its topic.
        topic: TableRef,
    },
    /// `ALTER GROUP 'billing' ON TOPIC events START AT 41` — the group next
    /// hands out the message after that position, and forgets what it holds in
    /// flight.
    AlterGroup {
        /// The group.
        name: String,
        /// Its topic.
        topic: TableRef,
        /// The position it is to have been given everything up to.
        start_at: Expr,
    },
    /// `ACK events FOR CONSUMER 'billing' AT 7, 8` — those messages are done.
    AckTopic {
        /// The topic.
        topic: TableRef,
        /// The group the messages were handed out by.
        consumer: String,
        /// The positions acknowledged.
        positions: Vec<Expr>,
    },
    /// `NACK events FOR CONSUMER 'billing' AT 7 DELAY 5s` — those messages are
    /// to be handed out again, now or after the delay.
    NackTopic {
        /// The topic.
        topic: TableRef,
        /// The group the messages were handed out by.
        consumer: String,
        /// The positions given back.
        positions: Vec<Expr>,
        /// How long before they may be handed out again.
        delay: Option<Duration>,
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
        /// Whether the collection's records expire: `DEFINE COLLECTION drafts
        /// EXPIRE` (ADR-0122 A1).
        expire: Option<TableExpiry>,
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
        /// Whether its index keeps each vector as one byte per component
        /// (`QUANTIZED`), rescored on the records' full vectors.
        quantized: bool,
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
        /// `PASSPHRASE '…'`: the vault's key is wrapped under this passphrase
        /// rather than under the store's master key (ADR-0093), so the store's
        /// passphrase never opens it. A literal, for
        /// [`StatementKind::UnsealVault`]'s reason.
        passphrase: Option<String>,
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
        /// What a search index stores beside its postings: `POSITIONS`,
        /// `OFFSETS`, `NO SCORE` (ADR-0100 D4). Each changes what a read costs
        /// and never what it answers.
        costs: SearchCosts,
        /// Whether the index holds the cells covering each record's geometry.
        spatial: bool,
        /// Whether the index holds the (path, leaf) pairs of each record's
        /// document, so `CONTAINS` with a document can be served (ADR-0116).
        containment: bool,
        /// The distance a vector index's graph is built with, when it is one.
        ///
        /// Carried as the word the author wrote rather than as a parsed kind,
        /// because which distances exist is the store's question and not the
        /// grammar's: a name the store does not know is refused where the store
        /// knows what it knows, with the span the author can see.
        vector: Option<Name>,
        /// Whether a vector index keeps each vector as one byte per component
        /// (`QUANTIZED`). Only after `VECTOR <distance>`: on any other kind it
        /// would describe storage that kind does not have.
        quantized: bool,
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
    /// `BACKUP` or `BACKUP FROM 42` — the store's log as a backup file, answered
    /// with or, after `TO '<name>'`, written into the node's backup folder.
    ///
    /// The one statement whose scope is the **store** rather than the selected
    /// namespace, which is why it needs an owner rather than a table permission:
    /// there is no table for a grant to name.
    Backup {
        /// The sequence the file starts at; absent means the whole log.
        from: Option<u64>,
        /// Which of the backup formats is asked for (ADR-0091).
        form: BackupForm,
        /// `TO '<name>'` — the file, inside the node's backup folder, the backup
        /// is written to; absent means the statement answers with the bytes.
        to: Option<String>,
        /// `OF NAMESPACE prod, prod.orders` — the part of the store a script
        /// carries; empty means the whole store.
        of: Vec<ReachRef>,
    },
    /// `RESTORE SCRIPT FROM '<name>'` — a script backup in the node's backup
    /// folder, run into this store where it creates only places that do not
    /// exist yet.
    Restore {
        /// The file, inside the node's backup folder.
        from: String,
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
    /// `DEFINE SEARCH knowledge ON notes FIELDS title WEIGHT 3, body ANALYZER english`
    /// — several fields of several tables ranked as one collection (ADR-0105).
    DefineSearch {
        /// The search's name, unique within the database.
        name: Name,
        /// The tables and the fields each contributes. Never empty.
        members: Vec<SearchMember>,
        /// The analyzer every member field and every query is read with.
        analyzer: Name,
        /// `STOPWORDS <set>`: words a query drops outside a quoted phrase.
        stopwords: Option<Name>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE SYNONYMS tech { db: ['database'] }` — query-time alternatives,
    /// a store-wide name like an analyzer's.
    DefineSynonyms {
        /// The set's name.
        name: Name,
        /// Each word and what else answers it, as written.
        entries: Vec<(String, Vec<String>)>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE STOPWORDS common ['the', 'a']` — words a query drops.
    DefineStopwords {
        /// The set's name.
        name: Name,
        /// The words, as written.
        words: Vec<String>,
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
        /// The password or the stored hash, as written. Prints as `<redacted>`.
        credential: Credential,
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
        /// The one thing about it that changes.
        change: NamespaceChange,
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
        /// `BALANCE LEADERSHIPS`: the store line's leader moves a placement
        /// off a node leading more lines than another voter (ADR-0113 D3).
        /// Absent is off, because the statement replaces the set.
        balance_leaderships: bool,
    },
    /// `REVOKE CERTIFICATE '<sha256>'` — a peer certificate refused by every
    /// node from the moment the catalog reaches it (ADR-0108 D6).
    RevokeCertificate {
        /// The certificate's SHA-256, as 64 lowercase hexadecimal digits.
        fingerprint: String,
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
        /// Where a client reaches it over the wire (`CLIENTS AT`), when said —
        /// the address a redirect names (ADR-0101). `endpoint` is the peer
        /// door, which a client cannot speak to.
        clients: Option<String>,
        /// Its HTTP base (`HTTP AT`), when said — the `Location` a `307` names.
        http: Option<String>,
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
        /// `PREFERRED` after the placement: the candidate a non-preferred
        /// leader of that range hands it to once caught up (G053 SG5b).
        preferred: bool,
        /// The one certificate allowed to bind this row (`FINGERPRINT`), its
        /// SHA-256 as 64 lowercase hexadecimal digits (ADR-0108 D9).
        fingerprint: Option<String>,
        /// The region the peer stands in (`REGION 'eu'`), when said — what a
        /// `LOCAL MAJORITY` counts its voters by (G057 C3).
        region: Option<String>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `CREATE JOIN TOKEN FOR REPLICA second EXPIRES 10m` — a one-time token
    /// that binds the named row to the node presenting it (ADR-0108 D9).
    CreateJoinToken {
        /// The row the token binds.
        replica: Name,
        /// How long it binds for.
        expires: Duration,
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
    /// `DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows …`
    ///
    /// The same declared ingestion as [`StatementKind::DefineConsumer`], with a
    /// topic of this store as the source (ADR-0087). The group read, the record
    /// writes and the acknowledgement commit in one transaction, so each message
    /// is applied to this store once — which is why there is no `FORMAT` (a
    /// message is already a value) and no brokers.
    DefineTopicConsumer {
        /// The consumer's catalog identity, unique across both kinds.
        name: Name,
        /// The topic to read, in the destination's database.
        topic: TableRef,
        /// The group it reads as, already declared on the topic.
        group: String,
        /// Which message field carries the record's identity.
        identity: FieldPath,
        /// Which message fields become which record fields; a field nobody
        /// named does not land.
        mapping: Vec<FieldMapping>,
        /// The table the records land in.
        destination: TableRef,
        /// What happens to a message that cannot be applied: halt, or hand it
        /// back to the group, whose dead letter keeps it.
        on_failure: OnFailure,
        /// How many members this declaration runs on each node; `None` is one.
        parallelism: Option<u32>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP TOPIC CONSUMER orders_in` — stops it and forgets the declaration.
    DropTopicConsumer {
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
    /// `DROP SEARCH knowledge` — its member indexes go with it.
    DropSearch {
        /// The search to undeclare.
        name: Name,
    },
    /// `DROP SYNONYMS tech` — refused while a search names it.
    DropSynonyms {
        /// The set to undeclare.
        name: Name,
    },
    /// `DROP STOPWORDS common` — refused while a search names it.
    DropStopwords {
        /// The set to undeclare.
        name: Name,
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
        /// `PRIORITY BY f`: a claim takes the greatest `f` first, ties in
        /// arrival order, records without a number in `f` last (G055 C8).
        priority: Option<Name>,
        /// `NOT BEFORE f`: a record whose `f` holds a datetime after now is not
        /// handed out yet — delayed delivery, the delay a value in the record.
        not_before: Option<Name>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE SERIES readings RETAIN 30d [TIME at]`
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
        /// `TIME <field>` — the `datetime` field each record's identity is
        /// minted from, so the table is ordered and aged by when the event
        /// happened rather than when it arrived (ADR-0088 §1). `None` keeps
        /// arrival time.
        time: Option<Name>,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DEFINE ROLLUP hourly FROM readings WINDOW 1h [BY sensor] COMPUTE
    /// count(*) AS n, sum(v) AS total RETAIN 365d` — per-window aggregates of an
    /// event-time series, kept in the writing transaction (ADR-0088 §6).
    DefineRollup {
        /// The rollup's own name — an event-time series ordered by `window`.
        name: Name,
        /// The series it folds.
        source: Name,
        /// The window width.
        window: Duration,
        /// The raw field a row is kept per.
        by: Option<Name>,
        /// What each row computes: the fold word, its field (`None` for
        /// `count(*)`) and the name it answers under.
        computes: Vec<(Name, Option<Name>, Name)>,
        /// How far back the rollup answers — required, like a series'.
        retain: Duration,
        /// Whether re-defining an existing name is accepted.
        if_not_exists: bool,
    },
    /// `DROP ROLLUP hourly`
    DropRollup {
        /// The name to remove.
        name: Name,
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
        /// `MATERIALIZED`: the read's answer is kept as records and brought
        /// current from the change feed (ADR-0109), rather than re-run on every
        /// read that names the view.
        materialized: bool,
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
    /// `DEFINE EVENT audit ON orders FOR UPDATE WHEN $after.total > 100 THEN
    /// CREATE log = { … }` — statements run after each write of a record of the
    /// table, in the writer's transaction, as the writer (ADR-0110).
    ///
    /// The condition and the body are kept as **source text**, for the reason
    /// a view keeps its read as text: a stored syntax tree would need a version
    /// every time the grammar grew. Both are parsed here, so an event that could
    /// never run is refused where it is written.
    DefineEvent {
        /// The event's name, unique on its table.
        name: Name,
        /// The table whose writes run it.
        table: TableRef,
        /// The writes that run it — all three when `FOR` is not written.
        on: Vec<tessari_types::WriteKind>,
        /// `WHEN`, as written: the body runs only where it holds.
        when: Option<String>,
        /// The statements, as written, without the braces.
        body: String,
        /// Whether re-defining an existing name is accepted rather than
        /// refused.
        if_not_exists: bool,
    },
    /// `DROP EVENT audit ON orders`
    DropEvent {
        /// The event to undefine.
        name: Name,
        /// The table it is defined on.
        table: TableRef,
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
    /// `ALTER REPLICA b LEADS SHARD prod.shop.orders 2` · `… LEADS NONE` ·
    /// `… AT '…'` · `… ROLES …` · `… CLIENTS AT '…'` · `… HTTP AT '…'`
    ///
    /// Amends one clause of a peer's row (ADR-0098, Q-892); the rest of the row
    /// stays as declared, the node it is bound to included.
    AlterReplica {
        /// The peer whose row changes.
        name: Name,
        /// What changes.
        change: super::ReplicaChange,
    },
    /// `ALTER STORE FINALIZE FORMAT`
    ///
    /// Raises the format the store holds to the one this build writes, on every
    /// replica; after it no older build opens the store (ADR-0118).
    FinalizeFormat,
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
    /// `ANALYZE TABLE users`
    ///
    /// Takes the statistics the planner estimates the table's value indexes by
    /// — entries, distinct values, the most common values and equi-depth
    /// buckets — from a walk of each index's entries, on the node that runs it.
    /// A statistic decides which path a read takes and never which records it
    /// returns, so a node keeps its own and nothing travels in the log; a
    /// serving node also refreshes them itself as they go stale.
    AnalyzeTable {
        /// The table whose indexes are summarised.
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
        /// `EXPIRE 30m` / `EXPIRE NONE` after the value: when the record stops
        /// being answered, or that it never does (ADR-0122 A2). Taken only by a
        /// table that declares expiry; absent keeps the instant the record has.
        expire: Option<WriteExpiry>,
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
        /// `EXPIRE 30m` / `EXPIRE NONE` after the value: when the record stops
        /// being answered, or that it never does (ADR-0122 A2). Taken only by a
        /// table that declares expiry; absent keeps the instant the record has.
        expire: Option<WriteExpiry>,
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
        /// `UNSEAL VAULT team WITH …`: one vault carrying its own passphrase
        /// (ADR-0093) rather than the store's master key.
        vault: Option<Name>,
        /// The passphrase, as written.
        passphrase: String,
        /// Where the statement sits, so a refusal can point at it without
        /// quoting what stands there.
        span: Span,
    },
    /// `CHANGE VAULT PASSPHRASE FROM '…' TO '…'` — a rekey (ADR-0092 D3).
    ///
    /// Both passphrases are string literals for [`StatementKind::UnsealVault`]'s
    /// reason. The master key stays what it was and is wrapped again under the
    /// new passphrase, so no secret is re-encrypted; a backup taken before the
    /// change still opens with the old one, because the root travels in the log.
    ChangeVaultPassphrase {
        /// `CHANGE VAULT team PASSPHRASE …`: one vault's own passphrase.
        vault: Option<Name>,
        /// The passphrase that opens the store now.
        current: String,
        /// The passphrase that will open it afterwards.
        new: String,
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
        /// `SEAL VAULT team`: one vault's own key rather than the master key.
        vault: Option<Name>,
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
        /// `EXPIRE 30m` / `EXPIRE NONE` after the value: when the record stops
        /// being answered, or that it never does (ADR-0122 A2). Taken only by a
        /// table that declares expiry; absent keeps the instant the record has.
        expire: Option<WriteExpiry>,
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
        /// `EXPIRE 30m` / `EXPIRE NONE` after the value: when the record stops
        /// being answered, or that it never does (ADR-0122 A2). Taken only by a
        /// table that declares expiry; absent keeps the instant the record has.
        expire: Option<WriteExpiry>,
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
    /// `SET sessions:'abc' = … [EXPIRE 30s]`
    Set {
        /// The key to write.
        target: RecordTarget,
        /// The whole value.
        value: Expr,
        /// When the key stops being answered: a duration from now or a
        /// datetime. Absent means the key never expires — and a `SET` without
        /// it clears an expiry the key had, since a write replaces the whole
        /// version (G035).
        expire: Option<Expr>,
        /// Write only when this holds, and answer whether it wrote (G035).
        condition: Option<SetCondition>,
    },
    /// `INCR counters:'hits' [BY 5]` — add to a number and answer the result
    /// (G035). A missing key counts from zero; an expiry the key had is kept.
    Incr {
        /// The key.
        target: RecordTarget,
        /// How much to add; absent adds one.
        by: Option<Expr>,
    },
    /// `EXPIRE sessions:'abc' 30s` — give an existing key an expiry, or move
    /// the one it has (G035). A duration that is zero or negative, or a datetime
    /// already passed, removes the key.
    Expire {
        /// The key.
        target: RecordTarget,
        /// A duration from now, or a datetime.
        at: Expr,
    },
    /// `PERSIST sessions:'abc'` — the key stops expiring (G035).
    Persist {
        /// The key.
        target: RecordTarget,
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
    /// `KEYS FROM sessions [RANGE 'a'..'m' | PREFIX 'user:'] [AFTER k] [LIMIT n]`
    Keys {
        /// The space to list.
        space: TableRef,
        /// The range of keys, when the statement bounds it.
        range: Option<RangeExpr>,
        /// The text every listed key begins with, when the statement says
        /// (G035). Exclusive with `range`: both name the stretch to walk.
        prefix: Option<Expr>,
        /// List only the keys after this one — the last key of the previous
        /// page (G035).
        after: Option<Expr>,
        /// At most this many keys (G035).
        limit: Option<u64>,
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

/// Which backup a `BACKUP` statement answers with (ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupForm {
    /// `BACKUP` — the store's log: every commit, in order.
    Log,
    /// `BACKUP STATE` — the store's current state at one version.
    State,
    /// `BACKUP SCRIPT` — the store's current state as TessariQL that rebuilds it.
    Script,
}

/// What a `SEARCH` index keeps beside its postings, as the statement said it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchCosts {
    /// `POSITIONS`: each posting carries the term's token ordinals.
    pub positions: bool,
    /// `OFFSETS`: each posting carries the term's byte ranges.
    pub offsets: bool,
    /// `NO SCORE`: no collection statistics, and a score over it is refused.
    pub unscored: bool,
}

/// One table of a `DEFINE SEARCH` and the fields it contributes (ADR-0105).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMember {
    /// The table.
    pub table: TableRef,
    /// Its fields, in the order written — the order a field's statistics are
    /// stored in.
    pub fields: Vec<SearchField>,
}

/// One field of a search, with what it allows and how much it weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchField {
    /// The field, possibly nested.
    pub path: FieldPath,
    /// `WEIGHT w`, when written: the field's BM25F weight. Absent is one.
    pub weight: Option<Number>,
    /// Whether a `MATCHES FUZZY` word may be answered here (`NO FUZZY` clears it).
    pub fuzzy: bool,
    /// Whether a prefix word may be answered here (`NO PREFIX` clears it).
    pub prefix: bool,
    /// Whether a quoted phrase may be answered here (`NO PHRASE` clears it).
    pub phrase: bool,
    /// `SYNONYMS <set>`: alternatives a word is also answered by, in this field.
    pub synonyms: Option<Name>,
    /// `SNIPPET`: the field a best window may be taken from.
    pub snippet: bool,
}

/// What a `FROM SEARCH` asks of the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchAsk {
    /// `MATCHES [PREFIX | FUZZY | INFIX] <query>` — the records, ranked.
    Matches {
        /// How the query's words are read.
        operator: SearchOperator,
        /// The query string.
        query: Box<Expr>,
    },
    /// `COMPLETE <beginning>` — the words the search holds that begin with it,
    /// ranked by how many records hold them.
    Complete {
        /// What was typed.
        beginning: Box<Expr>,
    },
}

/// How the words of a search query are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOperator {
    /// `MATCHES`: whole words, `OR`, `NOT`, quoted phrases and starred words.
    Words,
    /// `MATCHES PREFIX`: every word a beginning.
    Prefix,
    /// `MATCHES FUZZY`: every word within the edit budget.
    Fuzzy,
    /// `MATCHES INFIX`: every word a piece of a held word.
    Infix,
}
