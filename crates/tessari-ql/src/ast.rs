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
mod select;
mod statement;
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
    BackupForm, SearchAsk, SearchCosts, SearchField, SearchMember, SearchOperator, StatementKind,
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
    /// `INFO FOR SEARCH knowledge` — a declared search, its members and their
    /// statistics.
    Search(Name),
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
    /// `INFO FOR VAULT team RECORDS [AFTER team:'x'] [LIMIT n]` — the vault's
    /// record ids, a page at a time, and never a value (ADR-0092 D5).
    ///
    /// Its own subject rather than a clause on [`InfoSubject::Vault`], because
    /// it names the table for the grant check (`reach`) and the fields report
    /// names none. Ids only, because identities are keys and keys are not
    /// encrypted: this discloses nothing the key layout does not already.
    VaultRecords {
        /// The vault.
        table: TableRef,
        /// The last id of the page before.
        after: Option<Box<RecordTarget>>,
        /// How many ids at most; absent, a thousand.
        limit: Option<u64>,
    },
    /// `INFO FOR TOPIC events` — a topic's positions and its readers' (G037).
    Topic(TableRef),
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
    /// `INFO FOR SEAL` — whether this process can open secrets, and until when
    /// (ADR-0092 D1).
    ///
    /// A property of the **process**, not of any vault: the master key is held
    /// per process and so is the deadline it is held to. That is why it is its
    /// own subject rather than a field on `INFO FOR VAULT`, which would make a
    /// per-vault question out of a store-wide one. It names no table, so it
    /// needs nothing beyond being signed in.
    ///
    /// `INFO FOR SEAL OF team` asks about one vault (ADR-0093 D4): its own
    /// key's state when it carries its own passphrase, the store's when it does
    /// not, and which of the two with `custody`. It names a table, so it needs
    /// what `INFO FOR VAULT` needs.
    Seal(Option<Name>),
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
    /// `INFO FOR TOPIC CONSUMER orders_in` — one topic consumer's declaration,
    /// its running state on **this** node, and what it guarantees (ADR-0087).
    TopicConsumer(Name),
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

pub use tessari_types::BinaryOp;

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

/// What `DEFINE USER` is given to sign the user in with (ADR-0091).
///
/// A password is hashed before it is stored; a hash is stored as given, which is
/// how a state script re-creates a user it could never have known the password
/// of. Both print as `<redacted>`: a hash is not the password, but it is what an
/// offline guess is checked against, and nothing needs it in a log line.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// `PASSWORD '…'` — the plaintext, hashed on the way in.
    Password(Password),
    /// `PASSHASH '$argon2id$…'` — a hash this store would itself have produced.
    Hash(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Password(_) => "Password(<redacted>)",
            Self::Hash(_) => "Hash(<redacted>)",
        })
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `SPLIT AT 'm'` — the shard holding each point is retired and two are
    /// minted in its place, one point after another (ADR-0095). Records are
    /// not moved; writes from the commit onwards are filed by the new map.
    Split(Vec<RecordId>),
    /// `MERGE SHARD 4, 5` — two neighbouring live shards are retired and one is
    /// minted in their place, by the numbers `INFO FOR TABLE` reports.
    MergeShards(u32, u32),
    /// `SPLIT AUTOMATICALLY ABOVE 100000 RECORDS [OR 500 WRITES PER SECOND]
    /// MERGE BELOW 20000 RECORDS` — the store line's leader splits and merges
    /// the table's shards itself, each act an `ALTER TABLE` in the log
    /// (ADR-0113 D2).
    SplitAutomatically(AutoSplit),
    /// `SPLIT MANUALLY` — the shards change only by statement again.
    SplitManually,
}

/// When a table's shards are split and merged without being asked
/// (ADR-0113 D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSplit {
    /// A shard holding more records than this is split.
    pub above: u32,
    /// A shard taking more writes a second than this is split, however small.
    pub writes_per_second: Option<u32>,
    /// Two neighbouring shards holding fewer records than this together are
    /// merged.
    pub merge_below: u32,
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
mod tests;
