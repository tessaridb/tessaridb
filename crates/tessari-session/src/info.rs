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
//! Every value below comes from a `Catalog` reader. Nothing is cached, nothing
//! is written at declaration time to be read back here, and no statement text is
//! reconstructed — so a report cannot describe a schema the store no longer has.
//! An assertion is reported as the value the catalog stores, which is the
//! constraint itself rather than the sentence that once described it.

use std::collections::BTreeMap;

use tessari_ql::{InfoSubject, Name, Span, TableRef};
use tessari_storage::{
    BUILD_VERSION, Catalog, FieldDefinition, GrantDefinition, IndexDefinition, ReplicaDefinition,
    TableDefinition, Transaction, UserDefinition,
};
use tessari_types::{TableId, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::redact::Visible;
use crate::session::Session;

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
            InfoSubject::User(name) => self.info_user(transaction, name, span)?,
            InfoSubject::Node => self.info_node(transaction)?,
        };
        Ok(Outcome::Value(Value::Object(report)))
    }

    /// The namespaces.
    ///
    /// # The system tenancy is absent, and not because this filters it out
    ///
    /// Namespace zero holds the catalog and was never created through the
    /// language, so it has no definition record for [`Catalog::namespaces`] to
    /// find. It is unaddressable rather than hidden — the same property that
    /// makes `USE NAMESPACE <anything>` unable to select it. A listing that had
    /// to *remember* to exclude it would be one somebody could later forget to,
    /// which is exactly the change this statement was expected to bring.
    fn info_store(&self, transaction: &mut Transaction<'_>) -> Result<BTreeMap<String, Value>> {
        let own = self.identity.user().and_then(|user| user.namespace);
        let mut names = Vec::new();
        for namespace in Catalog::new(transaction).namespaces()? {
            // A user declared `ON prod.orders` belongs to one namespace and may
            // not name another, so the store as they may see it holds one.
            if own.is_some_and(|id| id != namespace.id) {
                continue;
            }
            names.push(namespace.name);
        }
        Ok(BTreeMap::from([("namespaces".to_owned(), by_name(names))]))
    }

    /// The databases in the selected namespace.
    fn info_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let namespace = self.namespace_id(transaction, span)?;
        let own = self.identity.user().and_then(|user| user.database);
        let mut names = Vec::new();
        for database in Catalog::new(transaction).databases_in(namespace)? {
            if own.is_some_and(|id| id != database.id) {
                continue;
            }
            names.push(database.name);
        }
        Ok(BTreeMap::from([("databases".to_owned(), by_name(names))]))
    }

    /// The tables in the selected database, narrowed to those this session may
    /// read.
    fn info_database(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let readable = self.readable_in(transaction)?;
        let mut names = Vec::new();
        for table in Catalog::new(transaction).tables_in(context.namespace, context.database)? {
            // A bucket's chunks live in a companion table whose name carries a
            // byte no identifier can hold, so no statement can name it and
            // `SELECT * FROM media` answers with files rather than chunks
            // (ADR-0011 §2). Listing it here would undo that in the one place
            // that enumerates rather than resolves.
            if !nameable(&table.name) {
                continue;
            }
            if readable
                .as_ref()
                .is_some_and(|granted| !granted.contains(&table.id))
            {
                continue;
            }
            names.push(table.name);
        }
        Ok(BTreeMap::from([("tables".to_owned(), by_name(names))]))
    }

    /// One table's shape, its fields and its indexes.
    ///
    /// The table itself is guarded before this runs: `INFO FOR TABLE` names its
    /// table, so `tables_named` hands it to the grant check and an ungranted
    /// caller is refused there, with the same message a `SELECT` from it gives.
    ///
    /// What is left is the **field** grant, which does not refuse — it edits.
    /// A caller granted `FIELDS name` reads records with `salary` already
    /// removed, so a report naming `salary` as a declared field would disclose
    /// what every read of theirs hides. The index list is filtered by the same
    /// rule and for the same reason: an index is named after the values it
    /// projects, so `by_salary ON staff FIELDS salary` says the field exists as
    /// plainly as the field list would.
    fn info_table(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
    ) -> Result<BTreeMap<String, Value>> {
        let (_, id) = self.resolve_table(transaction, table)?;
        let visible = self.visible_in(transaction, id)?;
        let catalog = Catalog::new(transaction);
        let Some(definition) = catalog.table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        let mut fields = catalog.fields_on(id)?;
        let mut indexes = catalog.indexes_on(id)?;
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        indexes.sort_by(|left, right| left.name.cmp(&right.name));
        let mut report = shape_of(&definition);
        report.insert(
            "fields".to_owned(),
            Value::Array(
                fields
                    .iter()
                    .filter(|field| readable_field(&visible, &field.name))
                    .map(described_field)
                    .collect(),
            ),
        );
        report.insert(
            "indexes".to_owned(),
            Value::Array(
                indexes
                    .iter()
                    .filter(|index| readable_index(&visible, index))
                    .map(described_index)
                    .collect(),
            ),
        );
        Ok(report)
    }

    /// One user's role, tenancy and grants.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, so every
    /// caller reaching here is an owner. Nothing is filtered: an owner asking
    /// what somebody may do gets the answer or the refusal, because a grant list
    /// with rows quietly removed would be read as the whole of what that user
    /// can reach.
    ///
    /// **The password hash is not in the report.** The stored definition carries
    /// it — `UserDefinition::to_value` writes it, because that value is what the
    /// catalog holds — so the report is built field by field rather than from
    /// that value. Reusing it would put every hash in the store onto the wire
    /// and into whatever logs the answer.
    fn info_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let catalog = Catalog::new(transaction);
        let Some(user) = catalog
            .users()?
            .into_iter()
            .find(|found| found.name == name.text)
        else {
            return Err(Error::Unknown {
                entity: "user",
                name: name.text.clone(),
                span,
            });
        };
        let grants = catalog.grants_for(user.id)?;
        let mut described = Vec::new();
        for grant in &grants {
            described.push(described_grant(&catalog, grant)?);
        }
        let mut report = described_user(&user);
        if let Some(id) = user.namespace
            && let Some(found) = catalog.namespace(id)?
        {
            report.insert("namespace".to_owned(), Value::from(found.name.as_str()));
        }
        if let Some(id) = user.database
            && let Some(found) = catalog.database(id)?
        {
            report.insert("database".to_owned(), Value::from(found.name.as_str()));
        }
        report.insert("grants".to_owned(), Value::Array(described));
        Ok(report)
    }

    /// This node's own settings, and the peers it knows.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, for the
    /// reason `$node` needs it: the subject names no table, so a grant loop
    /// passes over it vacuously, and neither half has a smaller truthful form.
    ///
    /// # The two groups are the answer, not a formatting choice
    ///
    /// The flat fields come from the `META` keyspace and describe **this
    /// machine**. Everything under `cluster` comes from the catalog and
    /// describes the **topology**. That is ADR-0018's line, and ADR-0020 §3 puts
    /// it in the shape of the answer on purpose: a reader has to be able to tell
    /// which fields would follow a backup and which would not, and flattening
    /// the two would make that a thing you have to remember rather than a thing
    /// you can see. The bad day it is remembered wrongly on is the one where
    /// last night's backup goes onto a fresh machine and two processes claim one
    /// identity.
    ///
    /// `membership` is reported and is deliberately **not** settable. It reads
    /// `alone` because that is a fact about this process; the moment a node
    /// joins a cluster, the *name* of that cluster is topology and belongs on
    /// the other side of the line. Deciding which side in one sentence, with no
    /// second node to test against, is the mistake ADR-0018 §3 already made once.
    fn info_node(&self, transaction: &mut Transaction<'_>) -> Result<BTreeMap<String, Value>> {
        let identity = self.store.node_identity()?;
        let peers = Catalog::new(transaction)
            .replicas()?
            .iter()
            .map(described_replica)
            .collect();
        Ok(BTreeMap::from([
            (
                "id".to_owned(),
                Value::from(identity.record_id().to_string().as_str()),
            ),
            (
                "roles".to_owned(),
                Value::Array(
                    identity
                        .roles
                        .names()
                        .into_iter()
                        .map(Value::from)
                        .collect(),
                ),
            ),
            (
                "membership".to_owned(),
                Value::from(identity.membership.name()),
            ),
            (
                "version".to_owned(),
                Value::from(identity.version.to_string().as_str()),
            ),
            // The exact build beside the ordered version, for the same reason
            // it sits beside it in `$node`: an operator holding a pre-release
            // has to be able to see that they are holding one.
            ("build".to_owned(), Value::from(BUILD_VERSION)),
            (
                "endpoints".to_owned(),
                Value::Array(
                    identity
                        .endpoints
                        .iter()
                        .map(|endpoint| Value::from(endpoint.as_str()))
                        .collect(),
                ),
            ),
            (
                "cluster".to_owned(),
                Value::Object(BTreeMap::from([("peers".to_owned(), Value::Array(peers))])),
            ),
        ]))
    }
}

/// One peer, as the catalog holds it.
///
/// An object rather than a bare endpoint, because a peer has a name an operator
/// wrote and an address they may change, and a list of addresses could not say
/// which one moved.
fn described_replica(replica: &ReplicaDefinition) -> Value {
    Value::Object(BTreeMap::from([
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

/// What a table declares about itself.
///
/// The three markers as the catalog holds them, rather than one word naming a
/// kind. A `DEFINE SPACE` and a plain `DEFINE TABLE` store the same markers, so
/// a report claiming to name the kind would be inventing a distinction the
/// catalog does not carry.
fn shape_of(definition: &TableDefinition) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("table".to_owned(), Value::from(definition.name.as_str())),
        ("schemafull".to_owned(), Value::Bool(definition.schemafull)),
        ("edge".to_owned(), Value::Bool(definition.edge)),
        ("bucket".to_owned(), Value::Bool(definition.bucket)),
    ])
}

/// One declared field.
fn described_field(field: &FieldDefinition) -> Value {
    let mut described = BTreeMap::from([
        ("name".to_owned(), Value::from(field.name.as_str())),
        ("type".to_owned(), Value::from(field.kind.name())),
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
    ]);
    if let Some(distance) = index.vector {
        described.insert("vector".to_owned(), Value::from(distance.name()));
    }
    Value::Object(described)
}

/// One user, without the secret.
fn described_user(user: &UserDefinition) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("user".to_owned(), Value::from(user.name.as_str())),
        ("role".to_owned(), Value::from(user.role.name())),
    ])
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
