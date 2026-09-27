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

use std::collections::BTreeMap;

use tessari_encoding::{SpatialRefinement, VectorRecall};
use tessari_ql::{
    Answer, Identity as RecordIdentity, InfoSubject, Name, Projection, RecordTarget, Select,
    Source, Span, StatementKind, TableRef,
};
use tessari_storage::{
    Catalog, ConsumerDefinition, FieldDefinition, FollowerLag, GrantDefinition, IndexDefinition,
    MEASURED_RELATION, Progress, Reach, ReplicaDefinition, TableDefinition, Transaction,
    UserDefinition,
};
use tessari_types::{DatabaseId, NamespaceId, Number, TableId, Value};

use crate::error::Result;
use crate::identity::Identity;
use crate::outcome::Outcome;
use crate::redact::Visible;
use crate::session::Session;

mod access;
mod cluster;
mod records;
mod structure;
mod vaults;

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
            InfoSubject::Geo(name) => self.info_geo(transaction, name, span)?,
            InfoSubject::Vault(name) => self.info_vault(transaction, name, span)?,
            InfoSubject::Topic(table) => self.info_topic(transaction, table, span)?,
            InfoSubject::Bucket(name) => self.info_bucket(transaction, name, span)?,
            InfoSubject::Recipients(target) => self.info_recipients(transaction, target, span)?,
            InfoSubject::Versions(target) => self.info_versions(transaction, target, span)?,
            InfoSubject::History(target) => self.info_history(transaction, target, span)?,
            InfoSubject::Audit(actor) => self.info_audit(actor.as_ref())?,
            InfoSubject::User(name) => self.info_user(transaction, name, span)?,
            InfoSubject::Users => self.info_users(transaction)?,
            InfoSubject::Access(table) => self.info_access(transaction, table, span)?,
            InfoSubject::Node => self.info_node(transaction)?,
            InfoSubject::Consumer(name) => self.info_consumer(transaction, name, span)?,
            InfoSubject::Consumers => self.info_consumers(transaction)?,
        };
        Ok(Outcome::Value(Value::Object(report)))
    }
}

/// What a consumer promises, and what it refuses to.
///
/// Built per answer rather than held in a constant, because a [`Value`] cannot
/// be one — and the cost is irrelevant: this runs once per administrative
/// statement, not once per record.
///
/// It is part of the report rather than of the documentation alone because the
/// failure being avoided is a documented one: the system that has shipped this
/// feature longest states its delivery guarantee in a guide and a design
/// proposal, and *not* on the page somebody reads while configuring a consumer.
/// The reader of this output is configuring one right now.
fn guarantees() -> Value {
    Value::Object(BTreeMap::from([
        ("delivery".to_owned(), Value::from("at-least-once")),
        (
            "idempotence".to_owned(),
            Value::from(
                "a replayed message converges to one record, because the identity field \
                 makes the write a compare-and-set",
            ),
        ),
        (
            "exactly_once".to_owned(),
            Value::from(
                "not offered: the store commit and the broker offset commit are two \
                 commits into two systems, and the store's comes first, which chooses \
                 duplicates over loss",
            ),
        ),
        (
            "schema".to_owned(),
            Value::from("declared, never inferred: a message field nobody mapped does not land"),
        ),
    ]))
}

/// One consumer's declaration, as an object.
fn described_consumer(consumer: &ConsumerDefinition, destination: &str) -> Value {
    let brokers = consumer
        .brokers
        .iter()
        .map(|broker| Value::from(broker.as_str()))
        .collect();
    let mapping = consumer
        .mapping
        .iter()
        .map(|pair| {
            Value::Object(BTreeMap::from([
                ("from".to_owned(), Value::from(pair.from.as_str())),
                ("to".to_owned(), Value::from(pair.to.as_str())),
            ]))
        })
        .collect();
    Value::Object(BTreeMap::from([
        ("name".to_owned(), Value::from(consumer.name.as_str())),
        ("brokers".to_owned(), Value::Array(brokers)),
        ("topic".to_owned(), Value::from(consumer.topic.as_str())),
        ("group".to_owned(), Value::from(consumer.group.as_str())),
        ("format".to_owned(), Value::from(consumer.format.as_str())),
        (
            "identity".to_owned(),
            Value::from(consumer.identity.as_str()),
        ),
        ("mapping".to_owned(), Value::Array(mapping)),
        ("destination".to_owned(), Value::from(destination)),
        (
            "on_failure".to_owned(),
            Value::from(consumer.on_failure.spelling()),
        ),
        (
            "parallelism".to_owned(),
            Value::Number(tessari_types::Number::Integer(i64::from(
                consumer.parallelism,
            ))),
        ),
        // Whose authority its writes carry. Reported as a **word** and not as
        // an absent field when there is none, because the absence is the one
        // an operator has to act on: a consumer declared before this existed
        // writes unbound, and a field that simply vanished would leave no way
        // to find which ones. `NULL` here would read as *no information*; this
        // reads as *nobody*, which is what it is.
        (
            "declarer".to_owned(),
            consumer.declarer.map_or_else(
                || Value::from("unbound — declared before writes carried an identity"),
                |id| Value::Number(tessari_types::Number::Integer(i64::from(id))),
            ),
        ),
    ]))
}

/// What this process is doing, or that it is doing nothing.
fn running_state(progress: Option<&Progress>) -> Value {
    let Some(progress) = progress else {
        // Named rather than left as an absent field, because "this node is not
        // running it" is the answer an operator is most often looking for, and
        // an empty object would read as "no information".
        return Value::Object(BTreeMap::from([("here".to_owned(), Value::Bool(false))]));
    };
    let positions = progress
        .positions
        .iter()
        .map(|(partition, offset)| {
            Value::Object(BTreeMap::from([
                (
                    "partition".to_owned(),
                    Value::Number(tessari_types::Number::Integer(i64::from(*partition))),
                ),
                (
                    "offset".to_owned(),
                    Value::Number(tessari_types::Number::Integer(*offset)),
                ),
            ]))
        })
        .collect();
    Value::Object(BTreeMap::from([
        ("here".to_owned(), Value::Bool(true)),
        (
            "applied".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(progress.applied).unwrap_or(i64::MAX),
            )),
        ),
        (
            "quarantined".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(progress.quarantined).unwrap_or(i64::MAX),
            )),
        ),
        (
            "last_error".to_owned(),
            progress
                .last_error
                .as_deref()
                .map_or(Value::Null, Value::from),
        ),
        ("positions".to_owned(), Value::Array(positions)),
    ]))
}

/// One peer, as the catalog holds it.
///
/// An object rather than a bare endpoint, because a peer has a name an operator
/// wrote and an address they may change, and a list of addresses could not say
/// which one moved.
fn described_replica(replica: &ReplicaDefinition, catalog: &Catalog<'_, '_>) -> Result<Value> {
    Ok(Value::Object(BTreeMap::from([
        ("name".to_owned(), Value::from(replica.name.as_str())),
        (
            "endpoint".to_owned(),
            Value::from(replica.endpoint.as_str()),
        ),
        // Reported because it is now *routing*, not decoration: this is the
        // field that decides where a forwarded write lands, and a setting an
        // operator can write but cannot read back is one they cannot check
        // before the bad day. Named the same way `$node` names its own roles,
        // so the two sides of the membership row read alike.
        (
            "roles".to_owned(),
            Value::Array(replica.roles.names().into_iter().map(Value::from).collect()),
        ),
        // Reported for the same reason `roles` is, one step further: a binding
        // an operator can write and cannot read back is one they cannot check,
        // and the mistake it hides is the quiet one — a row bound to the wrong
        // id names a node that does not exist, so nothing converges and nothing
        // complains. Rendered as the id's own spelling, which is what `id` above
        // prints and what the `NODE` clause reads back.
        (
            "node".to_owned(),
            replica.node.map_or(Value::Null, Value::Uuid),
        ),
        // The third of three, and the one with the quietest failure: a peer
        // subscribed to nothing receives nothing, and a cluster in that state
        // reports no error anywhere — every node is up, every greeting lands,
        // and one copy simply never changes. Written back in the spelling the
        // clause takes, so what this prints can be pasted into the statement
        // that would correct it.
        (
            "replicates".to_owned(),
            match replica.replicates {
                None => Value::Null,
                Some(reach) => Value::from(spelled_reach(reach, catalog)?.as_str()),
            },
        ),
        // The placement (ADR-0082), in the spelling `LEADS` takes, so the answer
        // to *which node stands for which range* is on the row that decides it.
        (
            "leads".to_owned(),
            match replica.leads {
                None => Value::Null,
                Some(reach) => Value::from(spelled_reach(reach, catalog)?.as_str()),
            },
        ),
    ])))
}

/// A subscription's reach, written the way the clause writes it.
///
/// Names and not ids: an id is a number the operator never typed and cannot act
/// on, and the whole reason to report a setting is that somebody can compare it
/// against what they meant. A name the catalog has lost is reported as the id it
/// could not resolve rather than omitted — a row pointing at a namespace that no
/// longer exists is precisely the state worth seeing.
fn spelled_reach(reach: Reach, catalog: &Catalog<'_, '_>) -> Result<String> {
    Ok(match reach {
        Reach::Store => "STORE".to_owned(),
        Reach::Namespace(namespace) => {
            format!("NAMESPACE {}", namespace_named(namespace, catalog)?)
        }
        Reach::Database(namespace, database) => {
            let held = catalog
                .databases_in(namespace)?
                .into_iter()
                .find(|found| found.id == database)
                .map_or_else(|| database.get().to_string(), |found| found.name);
            format!("DATABASE {}.{held}", namespace_named(namespace, catalog)?)
        }
        // `SHARD prod.shop.orders 2` — the clause's own spelling (G031), so the
        // report pastes back into the statement that would correct it.
        Reach::Shard(namespace, database, table, shard) => {
            let held = catalog
                .databases_in(namespace)?
                .into_iter()
                .find(|found| found.id == database)
                .map_or_else(|| database.get().to_string(), |found| found.name);
            let named = catalog
                .table(table)?
                .map_or_else(|| table.get().to_string(), |found| found.name);
            format!(
                "SHARD {}.{held}.{named} {}",
                namespace_named(namespace, catalog)?,
                shard.get()
            )
        }
    })
}

/// One namespace's name, or its id when the catalog no longer holds it.
fn namespace_named(namespace: NamespaceId, catalog: &Catalog<'_, '_>) -> Result<String> {
    Ok(catalog
        .namespace(namespace)?
        .map_or_else(|| namespace.get().to_string(), |found| found.name))
}

/// One follower, as the leader has experienced it.
///
/// Both units, because each has a blind spot the other covers. A follower that
/// stopped collecting while this leader was idle is behind by nothing at all —
/// `behind` reads zero and it looks well, because in sequences it *is* well;
/// only `quiet_for` grows. A follower collecting steadily but unable to keep up
/// has almost no `quiet_for`; only `behind` grows. That is why PostgreSQL
/// publishes positions and lags from the primary rather than either alone.
///
/// `quiet_for` is time since this follower last collected. `copy_age` is the
/// other question — how old the data it holds is — and the two come apart on an
/// idle leader, where a perfectly level follower's `quiet_for` grows for as long
/// as there is nothing to collect while its copy stays current. `copy_age` is
/// read against the leader's own timeline of its tail, so it is an upper bound
/// overstating by at most one sampling interval, and it is `null` when the copy
/// predates everything this leader has sampled — beyond every bound, not zero.
/// A failover policy as a report shows it: the five periods and the pair.
///
/// The periods are durations rather than numbers, because the store has a
/// duration type and an integer here would put the unit in a doc comment
/// somewhere else. The pair is beside them rather than inside them: `epoch` and
/// `version` are not periods, they are which policy this is, and a reader
/// comparing two nodes compares the pair first.
fn described_failover(held: &tessari_storage::FailoverDefinition) -> Value {
    let period = |span: std::time::Duration| {
        tessari_types::Duration::new(
            i64::try_from(span.as_secs()).unwrap_or(i64::MAX),
            span.subsec_nanos(),
        )
        .map_or(Value::Null, Value::Duration)
    };
    Value::Object(BTreeMap::from([
        ("awareness".to_owned(), period(held.policy.awareness())),
        ("collection".to_owned(), period(held.policy.collection())),
        ("round".to_owned(), period(held.policy.round())),
        ("campaign".to_owned(), period(held.policy.campaign())),
        ("lease".to_owned(), period(held.policy.lease())),
        (
            "epoch".to_owned(),
            Value::from(i64::try_from(held.epoch.get()).unwrap_or(i64::MAX)),
        ),
        (
            "version".to_owned(),
            Value::from(i64::try_from(held.version).unwrap_or(i64::MAX)),
        ),
    ]))
}

fn described_follower(lag: FollowerLag) -> Value {
    Value::Object(BTreeMap::from([
        ("node".to_owned(), Value::Uuid(lag.node)),
        (
            "sequence".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(lag.sequence.get()).unwrap_or(i64::MAX),
            )),
        ),
        (
            "behind".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(lag.behind).unwrap_or(i64::MAX),
            )),
        ),
        (
            "quiet_for".to_owned(),
            tessari_types::Duration::new(
                i64::try_from(lag.quiet_for.as_secs()).unwrap_or(i64::MAX),
                lag.quiet_for.subsec_nanos(),
            )
            .map_or(Value::Null, Value::Duration),
        ),
        (
            "copy_age".to_owned(),
            lag.copy_age.map_or(Value::Null, |age| {
                tessari_types::Duration::new(
                    i64::try_from(age.as_secs()).unwrap_or(i64::MAX),
                    age.subsec_nanos(),
                )
                .map_or(Value::Null, Value::Duration)
            }),
        ),
    ]))
}

/// A measured refinement, reported with everything needed to read it.
///
/// The two ratios are given as percentages **and** the counts they came from are
/// given beside them, because the ratios answer different questions and a reader
/// who only trusts one of them should be able to recompute it. `refinement` is
/// how loose the boxes are — records offered per record kept. `fragmentation` is
/// how many entries the traversal reads per record it arrives at, which is the
/// separate failure of one record occupying many cells.
///
/// Both are `none` rather than zero when there was nothing to divide by, for the
/// reason a recall is: a zero here would read as a perfect filter.
///
/// `relation` says which query the figures answer for, and it is not decoration.
/// The measurement asks the widest relation there is, so it is the one that
/// exposes a loose covering — and a store only ever read with a narrower one
/// refines a smaller set at a cost this figure does not describe. Without the
/// label that scope is invisible: the reader sees `refinement` and has no way to
/// learn it means *refinement under `meets`*.
fn refining(measured: SpatialRefinement) -> Value {
    let percentage = |held: Option<u64>| {
        held.map_or(Value::None, |value| {
            Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
        })
    };
    let count = |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
    Value::Object(BTreeMap::from([
        ("relation".to_owned(), Value::from(MEASURED_RELATION.name())),
        ("refinement".to_owned(), percentage(measured.refinement())),
        (
            "fragmentation".to_owned(),
            percentage(measured.fragmentation()),
        ),
        ("entries".to_owned(), count(measured.entries)),
        ("reached".to_owned(), count(measured.reached)),
        ("admitted".to_owned(), count(measured.admitted)),
        (
            "sample".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.sample))),
        ),
        ("records".to_owned(), count(measured.records)),
    ]))
}

/// A measured recall, reported with everything needed to read it.
///
/// Never the percentage alone. Recall decays as records are added after the
/// build that measured it, so a lone figure describes a store that may no longer
/// exist — `records` is what lets a reader see the store has outgrown it, and
/// `at`, `sample` and the two constants say what was actually measured.
fn reported(measured: VectorRecall) -> Value {
    Value::Object(BTreeMap::from([
        (
            "recall".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.recall))),
        ),
        (
            "at".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.at))),
        ),
        (
            "sample".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.sample))),
        ),
        (
            "records".to_owned(),
            Value::Number(Number::Integer(
                i64::try_from(measured.records).unwrap_or(i64::MAX),
            )),
        ),
        (
            "neighbours".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.neighbours))),
        ),
        (
            "exploration".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.exploration))),
        ),
    ]))
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
fn nameable(name: &str) -> bool {
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

/// What a table declares about itself.
///
/// The three markers as the catalog holds them, rather than one word naming a
/// kind. A `DEFINE SPACE` and a plain `DEFINE TABLE` store the same markers, so
/// a report claiming to name the kind would be inventing a distinction the
/// catalog does not carry.
fn shape_of(definition: &TableDefinition) -> BTreeMap<String, Value> {
    let mut shape = BTreeMap::from([
        ("table".to_owned(), Value::from(definition.name.as_str())),
        ("schemafull".to_owned(), Value::Bool(definition.schemafull)),
        ("edge".to_owned(), Value::Bool(definition.is_edge())),
        ("bucket".to_owned(), Value::Bool(definition.is_bucket())),
        // Reported because it is **stored** and behaves like nothing else in the
        // report: a collection and a `SCHEMALESS` table accept the same writes,
        // so a report that omitted this would describe the two identically and a
        // declaration rebuilt from it would silently lose the word.
        (
            "collection".to_owned(),
            Value::Bool(definition.is_collection()),
        ),
        // Reported for the same reason and one stronger: a vault and a plain
        // table accept the same declarations to look at, so a report omitting
        // this describes them identically — and the one the report is about
        // refuses `SELECT`, seals its `SECRET` fields and cannot be made
        // schemaless. A declaration rebuilt from a report without it loses the
        // word `VAULT`, which is the word that mints the key.
        ("vault".to_owned(), Value::Bool(definition.is_vault())),
        // Reported for the same reason, and one more: it decides what the *next*
        // unnamed write is called, so a table read back without it looks like
        // every other table right up until a record is created under a scheme
        // nobody asked for.
        (
            "identity".to_owned(),
            Value::from(definition.identity.name()),
        ),
        // G027 S4.1, and reported for the reason `collection` and `vault` above
        // are, with the consequence one step further out. A table that declares
        // `LAST WRITER WINS` and one that declares nothing accept the same
        // writes and differ only in what happens to a write they cannot order:
        // the first takes it and counts the loss, the second refuses and names
        // both versions. Omit this and the two describe themselves identically,
        // and the declaration rebuilt below loses the words that decide which
        // one it is.
        //
        // `NONE` where nothing was declared, rather than an absent key: silence
        // is a refusal by decision (ADR-0075), not by default, and a report that
        // says nothing about it cannot be distinguished from one taken off a
        // build that had never heard of the clause.
        (
            "conflict".to_owned(),
            definition
                .conflict
                .map_or(Value::None, tessari_types::ConflictPolicy::to_value),
        ),
    ]);
    // Present only on a view, and it carries the read rather than a flag. A
    // marker alone would say the least useful true thing: two views differ
    // entirely in what they answer and not at all in being views, so a report
    // omitting the read describes every view identically. It is the same reason
    // the endpoint pair below is reported and not merely the edge flag.
    if let Some(read) = definition.view_read() {
        shape.insert("view".to_owned(), Value::from(read));
    }
    // Present only on a split table (G031, ADR-0080). Each bound is the literal
    // the clause takes — `'g'`, `uuid '…'` — so what the report prints is what
    // the next declaration types, and `NONE` marks an open end rather than a
    // shard with nothing in it.
    if let Some(shards) = &definition.shards {
        let bound = |at: Option<&tessari_types::RecordId>| {
            at.map_or(Value::None, |id| Value::from(id.to_literal().as_str()))
        };
        shape.insert(
            "shards".to_owned(),
            Value::Array(
                shards
                    .spans()
                    .map(|span| {
                        Value::Object(BTreeMap::from([
                            ("id".to_owned(), Value::from(i64::from(span.id.get()))),
                            ("from".to_owned(), bound(span.from)),
                            ("to".to_owned(), bound(span.to)),
                        ]))
                    })
                    .collect(),
            ),
        );
    }
    // Present only on a table that belongs to one, and reported as the **id**
    // for the reason the endpoints below are: this report says what is stored,
    // and a name resolved here would be a second read able to disagree with the
    // first. `INFO FOR GRAPH` is the reverse direction and takes the name.
    if let Some(graph) = definition.graph {
        shape.insert("graph".to_owned(), Value::from(i64::from(graph.get())));
    }
    // Present only on an edge table that declared its pair, and it has to be
    // present there: the endpoints and the order are the whole of what the
    // clause adds, and a declared pair reported as a bare edge table would be
    // described identically to one that accepts writes it refuses — the same
    // failure the `collection` marker above exists to prevent, one clause
    // further on.
    //
    // The endpoints are reported as **table ids**, because that is what the
    // catalog holds and this report says what is stored. Resolving them to names
    // would be a second read that can disagree with the first.
    if let Some(endpoints) = definition.edge_endpoints() {
        let mut declared = BTreeMap::from([
            (
                "from".to_owned(),
                Value::from(i64::from(endpoints.from.get())),
            ),
            ("to".to_owned(), Value::from(i64::from(endpoints.to.get()))),
        ]);
        if let Some(order) = &endpoints.order {
            declared.insert("order".to_owned(), Value::from(order.field.as_str()));
            declared.insert("descending".to_owned(), Value::Bool(order.descending));
        }
        shape.insert("endpoints".to_owned(), Value::Object(declared));
    }
    shape
}

/// One declared field.
fn described_field(field: &FieldDefinition) -> Value {
    let mut described = BTreeMap::from([
        ("name".to_owned(), Value::from(field.name.as_str())),
        ("type".to_owned(), Value::from(field.kind.name().as_ref())),
        ("required".to_owned(), Value::Bool(field.required)),
    ]);
    if let Some(default) = &field.default {
        described.insert("default".to_owned(), Value::from(default.as_str()));
    }
    if let Some(analyzer) = &field.analyzer {
        described.insert("analyzer".to_owned(), Value::from(analyzer.as_str()));
    }
    if let Some(assert) = &field.assert {
        // The stored constraint, not a sentence describing it — the catalog
        // holds the lowered form and this is it.
        described.insert("assert".to_owned(), assert.to_value());
    }
    Value::Object(described)
}

/// One declared index.
fn described_index(index: &IndexDefinition) -> Value {
    let mut described = BTreeMap::from([
        ("name".to_owned(), Value::from(index.name.as_str())),
        (
            "fields".to_owned(),
            Value::Array(
                index
                    .fields
                    .iter()
                    .map(|path| Value::String(path.to_string()))
                    .collect(),
            ),
        ),
        ("unique".to_owned(), Value::Bool(index.unique)),
        ("search".to_owned(), Value::Bool(index.search)),
        // The fourth kind. It was missing here while the catalog has carried it
        // all along, so a spatial index read back as an ordinary one — a report
        // that said the index answers ranges when it answers cells.
        ("spatial".to_owned(), Value::Bool(index.spatial)),
    ]);
    if let Some(distance) = index.vector {
        described.insert("vector".to_owned(), Value::from(distance.name()));
    }
    Value::Object(described)
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

/// One user, without the secret.
fn described_user(user: &UserDefinition) -> BTreeMap<String, Value> {
    let mut described = BTreeMap::from([("user".to_owned(), Value::from(user.name.as_str()))]);
    // Absent rather than a placeholder when no role summarises what the user
    // holds. A listing that printed `viewer` there would be describing an
    // authority they do not have, and the field below is the true answer.
    if let Some(role) = user.role {
        described.insert("role".to_owned(), Value::from(role.name()));
    }
    described
}

/// One user's authorities, with every reach named rather than numbered.
///
/// Named because a numbered reach is unreadable to the person who has to decide
/// whether it is right, and deciding that is the only reason to ask. Until the
/// enforcement wave lands this is also the **only** observable effect a grant
/// has, so a report without it would leave a grant unverifiable.
fn described_authorities(catalog: &Catalog<'_, '_>, user: &UserDefinition) -> Result<Value> {
    let mut described = Vec::new();
    for held in user.authorities.iter() {
        let reach = match held.reach {
            Reach::Store => "store".to_owned(),
            Reach::Namespace(namespace) => named_namespace(catalog, namespace)?,
            Reach::Database(namespace, database) => format!(
                "{}.{}",
                named_namespace(catalog, namespace)?,
                named_database(catalog, database)?
            ),
            // Never held — no authority comes at a shard's reach — and reported
            // faithfully if a stored row ever says so, because a report that
            // hid it would hide exactly the row worth seeing.
            Reach::Shard(namespace, database, table, shard) => format!(
                "{}.{}.{} shard {}",
                named_namespace(catalog, namespace)?,
                named_database(catalog, database)?,
                catalog
                    .table(table)?
                    .map_or_else(|| table.get().to_string(), |found| found.name),
                shard.get()
            ),
        };
        described.push(Value::Object(BTreeMap::from([
            ("authority".to_owned(), Value::from(held.kind.name())),
            ("reach".to_owned(), Value::from(reach.as_str())),
        ])));
    }
    Ok(Value::Array(described))
}

/// A namespace's name, or its number when the definition is gone.
///
/// A dropped namespace can still be named by an authority somebody holds, and
/// the number is a truthful answer where inventing a name would not be.
fn named_namespace(catalog: &Catalog<'_, '_>, namespace: NamespaceId) -> Result<String> {
    Ok(catalog
        .namespace(namespace)?
        .map_or_else(|| namespace.get().to_string(), |found| found.name))
}

/// A database's name, on the same terms.
fn named_database(catalog: &Catalog<'_, '_>, database: DatabaseId) -> Result<String> {
    Ok(catalog
        .database(database)?
        .map_or_else(|| database.get().to_string(), |found| found.name))
}

/// One grant, with the table named rather than numbered.
fn described_grant(catalog: &Catalog<'_, '_>, grant: &GrantDefinition) -> Result<Value> {
    let named = table_named(catalog, grant.table)?;
    Ok(Value::Object(BTreeMap::from([
        ("table".to_owned(), named),
        (
            "verbs".to_owned(),
            Value::Array(
                grant
                    .verbs
                    .iter()
                    .map(|verb| Value::from(verb.name()))
                    .collect(),
            ),
        ),
        (
            "fields".to_owned(),
            Value::Array(
                grant
                    .fields
                    .iter()
                    .map(|field| Value::from(field.as_str()))
                    .collect(),
            ),
        ),
    ])))
}

/// A table's name, or nothing when its definition has been dropped.
///
/// A grant outlives the table it names — dropping a table removes the definition
/// and leaves the grant — so this is an absence the report has to be able to
/// say rather than an error it can raise.
fn table_named(catalog: &Catalog<'_, '_>, table: TableId) -> Result<Value> {
    Ok(catalog
        .table(table)?
        .map_or(Value::None, |found| Value::from(found.name.as_str())))
}
