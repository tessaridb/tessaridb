//! `INFO FOR` — reading the catalog, and reporting only what the asker may see.
//!
//! # The rule the whole module follows
//!
//! **A report says only what the caller could have found out anyway.**
//!
//! That is not a new rule invented for this statement. [`Session::readable`]
//! states it for the change feed in its own words: a grant-governed subscriber
//! watching everything should see everything they were granted, which is what
//! the same user's `SELECT` per table would answer. Applied to a *description*
//! instead of to records, it decides every filter below.
//!
//! So four of the five subjects **filter** rather than refuse: a caller with a
//! grant on `orders` and none on `payroll` gets a database report listing
//! `orders`. The fifth, `INFO FOR USER`, refuses — because its content is the
//! permission system itself, and a partial account of who may do what reads as
//! the whole account.
//!
//! # Why the filters are here rather than in `within_grants`
//!
//! Three of the subjects name no table, so the grant check "every table this
//! statement names is granted" passes over them **vacuously** — the shape that
//! let a grant-governed owner take a whole backup until it was refused by name
//! (`reach.rs` records this at the arm itself). A refusal is the right answer
//! there because a backup has no smaller truthful form. A description does: the
//! subset the caller may read. So the answer here is the narrowing, and this
//! module is the single place it happens.
//!
//! # Read from the catalog, never from a rendering kept beside it
//!
//! Every value below comes from a `Catalog` reader. Nothing is cached and
//! nothing is written at declaration time to be read back here, so a report
//! cannot describe a schema the store no longer has. An assertion is reported as
//! the value the catalog stores, which is the constraint itself rather than the
//! sentence that once described it.
//!
//! `INFO FOR TABLE` also carries a `definition` — the declaration written back
//! out as TessariQL — and that is the same rule rather than an exception to it.
//! The text is rendered from the catalog **at the moment of the read**, from the
//! very lists this report is built from, and it is never a rendering kept beside
//! the definition to be handed back later. `describe.rs` holds the rendering and
//! the rule that governs it: nothing is written on a guess, so a declaration
//! with a part that has no faithful spelling is withheld and the part is named.

use tessari_ql::{
    Answer, Identity as RecordIdentity, InfoSubject, Name, Projection, RecordTarget, Select,
    Source, Span, StatementKind, TableRef,
};
use tessari_storage::{IndexDefinition, Transaction, UserDefinition};
use tessari_types::{DatabaseId, NamespaceId, Value};

use crate::error::Result;
use crate::identity::Identity;
use crate::outcome::Outcome;
use crate::redact::Visible;
use crate::session::Session;
pub(crate) use consumers::{described_consumer, guarantees, running_state};
pub(crate) use measures::{refining, reported};
pub(crate) use replicas::{
    described_failover, described_follower, described_leaders, described_replica,
};
pub(crate) use shapes::{described_field, described_index, shape_of};
pub(crate) use users::{
    described_authorities, described_grant, described_user, named_database, named_namespace,
};

mod access;
mod cluster;
mod consumers;
mod measures;
mod records;
mod replicas;
mod shapes;
mod structure;
mod topic_consumers;
mod users;
mod vaults;
pub(crate) use vaults::id_value;

impl Session<'_> {
    /// Report what the catalog holds about one subject.
    pub(crate) fn info(
        &self,
        transaction: &mut Transaction<'_>,
        subject: &InfoSubject,
        span: Span,
    ) -> Result<Outcome> {
        let report = match subject {
            InfoSubject::Store => self.info_store(transaction)?,
            InfoSubject::Namespace => self.info_namespace(transaction, span)?,
            InfoSubject::Database => self.info_database(transaction, span)?,
            InfoSubject::Table(table) => self.info_table(transaction, table)?,
            InfoSubject::Graph(name) => self.info_graph(transaction, name, span)?,
            InfoSubject::Vector(name) => self.info_vector(transaction, name, span)?,
            InfoSubject::Search(name) => self.info_search(transaction, name, span)?,
            InfoSubject::Geo(name) => self.info_geo(transaction, name, span)?,
            InfoSubject::Vault(name) => self.info_vault(transaction, name, span)?,
            InfoSubject::VaultRecords {
                table,
                after,
                limit,
            } => self.info_vault_records(transaction, table, after.as_deref(), *limit, span)?,
            InfoSubject::Topic(table) => self.info_topic(transaction, table, span)?,
            InfoSubject::Bucket(name) => self.info_bucket(transaction, name, span)?,
            InfoSubject::Recipients(target) => self.info_recipients(transaction, target, span)?,
            InfoSubject::Versions(target) => self.info_versions(transaction, target, span)?,
            InfoSubject::History(target) => self.info_history(transaction, target, span)?,
            InfoSubject::Audit(actor) => self.info_audit(actor.as_ref())?,
            InfoSubject::Seal(None) => self.info_seal(transaction)?,
            InfoSubject::Seal(Some(vault)) => self.info_seal_of(transaction, vault, span)?,
            InfoSubject::User(name) => self.info_user(transaction, name, span)?,
            InfoSubject::Users => self.info_users(transaction)?,
            InfoSubject::Access(table) => self.info_access(transaction, table, span)?,
            InfoSubject::Node => self.info_node(transaction)?,
            InfoSubject::Consumer(name) => self.info_consumer(transaction, name, span)?,
            InfoSubject::Consumers => self.info_consumers(transaction)?,
            InfoSubject::TopicConsumer(name) => {
                self.info_topic_consumer(transaction, name, span)?
            }
        };
        Ok(Outcome::Value(Value::Object(report)))
    }
}

/// A list of names, in name order.
///
/// The catalog hands these back in **id** order, which is the order somebody
/// happened to declare them in. Sorted, for the reason a grant's field list is
/// sorted: a report whose shape depends on the order a script was written in is
/// two answers to one question, and two stores built by different scripts from
/// the same schema would describe themselves differently.
fn by_name(mut names: Vec<String>) -> Value {
    names.sort();
    Value::Array(names.into_iter().map(Value::String).collect())
}

/// Whether a name is one a statement could have written.
///
/// An identifier is letters, digits and underscores, so a table whose name
/// carries anything else was created by the store for its own use and is
/// unreachable through the language.
///
/// **This restates a rule that belongs to the lexer**, which is the coupling to
/// watch: if an identifier ever admits another character, a table the language
/// can now name goes on being hidden here, silently. The rule is not moved down
/// today because doing it properly means a shared identifier predicate below
/// both crates (ADR-0012's shape), which is a change about that rule rather than
/// about this statement. Recorded as a question rather than half-built.
pub(crate) fn nameable(name: &str) -> bool {
    name.chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether this session may read a field, given what its grant names.
fn readable_field(visible: &Visible, name: &str) -> bool {
    visible.as_ref().is_none_or(|names| names.contains(name))
}

/// Whether this session may be told an index exists.
///
/// Only when **every** value it projects is readable. A composite index naming
/// one hidden field among four still names it.
fn readable_index(visible: &Visible, index: &IndexDefinition) -> bool {
    index
        .fields
        .iter()
        .all(|path| readable_field(visible, path.root()))
}

/// `USE NAMESPACE <ns> DATABASE <db>` — getting to where the table is.
///
/// The step a report about a table is most likely to leave out, and the one that
/// carries the tenancy rule: a user declared in another namespace is stopped
/// here and nowhere later, because every check after this one reads a selection
/// that has already been made.
fn selecting(namespace: Option<&str>, database: Option<&str>, span: Span) -> StatementKind {
    let named = |text: Option<&str>| {
        text.map(|text| Name {
            text: text.to_owned(),
            span,
        })
    };
    StatementKind::Use {
        namespace: named(namespace),
        database: named(database),
        // Synthesised to re-select tenancy and nothing else; a report never
        // declares a claimant.
        consumer: None,
    }
}

/// `SELECT * FROM <table>` — the ordinary read, as a statement to be judged.
///
/// Built rather than rendered and re-parsed. The object arrives here as a
/// [`TableRef`] the parser already produced, and turning it back into text would
/// give the one statement whose answer must not depend on spelling a quoting
/// rule to get wrong.
fn reading(table: &TableRef) -> StatementKind {
    StatementKind::Select(Box::new(Select {
        projection: Projection::All,
        omit: Vec::new(),
        from: Source::Table(table.clone()),
        // The guard applies to this read as it does to any other: a statement
        // the store builds for itself gets no privilege a caller could not ask
        // for in writing.
        lift_scan_guard: false,
        only: None,
        fetch: Vec::new(),
        split: None,
        group: Vec::new(),
        fill: None,
        latest: None,
        order: Vec::new(),
        fusion: None,
        after: None,
        approximate: None,
        start: None,
        limit: None,
        using: None,
        timeout: None,
        version: None,
        // This statement is never sent anywhere: it exists to be judged against
        // a grant. A tolerance for how stale an answering node may be has no
        // bearing on whether the read would be permitted.
        staleness: None,
        // Nor an answerer, for the same reason: which node answers has no
        // bearing on whether the read would be permitted.
        answered_by: None,
        span: table.span,
    }))
}

/// `DELETE <table>:0` — the ordinary write, as a statement to be judged.
///
/// A delete rather than a create, because a create carries a value and this is
/// never executed: the record id is a placeholder for a shape, and choosing the
/// verb with the least payload keeps that obvious.
fn writing(table: &TableRef) -> StatementKind {
    StatementKind::Delete {
        target: RecordTarget {
            table: table.clone(),
            id: RecordIdentity::Fixed(tessari_types::RecordId::Int(0)),
            span: table.span,
        },
        answer: Answer::Nothing,
    }
}

impl Session<'_> {
    /// Whether this caller administers the tenancy this user belongs to.
    ///
    /// The one place the boundary is computed, so that reading about somebody,
    /// listing them, changing them, removing them and granting to them cannot
    /// come to different answers. They did: the listing filtered and the lookup
    /// did not, so a name nobody would show you was a name you could still read
    /// every grant of — and three further statements had no check at all.
    pub(crate) fn administers(&self, user: &UserDefinition) -> bool {
        self.may_reach(user.namespace, user.database)
    }

    /// Whether this caller may act on something held at that tenancy.
    ///
    /// Takes the pair rather than a user, because the question is asked about a
    /// tenancy that **does not exist yet** as well as about one that does:
    /// `DEFINE USER` names a reach for somebody who is about to be created, and
    /// bounding only the statements that change an existing user leaves the
    /// obvious way round — mint a wider user, then be them. An owner of one
    /// database was able to declare an owner of the whole node, which made every
    /// other check on this page decorative.
    pub(crate) fn may_reach(
        &self,
        namespace: Option<NamespaceId>,
        database: Option<DatabaseId>,
    ) -> bool {
        match &self.identity {
            // An open store has no users to hide behind; a closed one refuses an
            // anonymous caller long before here. Reaching this with nobody
            // signed in therefore means the store is open, and an open store
            // hides nothing from anybody. It is also how the **first** user is
            // declared, which is the one moment nobody is signed in and a
            // store-wide owner must be creatable.
            Identity::Anonymous => true,
            Identity::Signed(who) => within(who.namespace, who.database, namespace, database),
        }
    }
}

/// Whether a caller holding the first tenancy may act on the second.
///
/// Containment, not equality, and asymmetric on purpose: the whole store
/// contains every namespace, a namespace contains its databases, and nothing
/// contains a sibling. `None` on the left is the node's administrator and
/// contains everything; `None` on the right is the whole store and is contained
/// by nobody but them.
pub(crate) fn within(
    namespace: Option<NamespaceId>,
    database: Option<DatabaseId>,
    their_namespace: Option<NamespaceId>,
    their_database: Option<DatabaseId>,
) -> bool {
    match (namespace, database) {
        (None, _) => true,
        (Some(held), None) => their_namespace == Some(held),
        (Some(held), Some(under)) => their_namespace == Some(held) && their_database == Some(under),
    }
}
