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
    BUILD_VERSION, Catalog, ConsumerDefinition, FieldDefinition, GEO_FIELD, GrantDefinition,
    IndexDefinition, MEASURED_RELATION, Progress, Reach, ReplicaDefinition, TableDefinition,
    TableKind, Transaction, UserDefinition,
};
use tessari_types::{DatabaseId, NamespaceId, Number, TableId, Value};

use crate::describe;
use crate::error::{Error, Result};
use crate::identity::Identity;
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
            InfoSubject::Graph(name) => self.info_graph(transaction, name, span)?,
            InfoSubject::Vector(name) => self.info_vector(transaction, name, span)?,
            InfoSubject::Geo(name) => self.info_geo(transaction, name, span)?,
            InfoSubject::Vault(name) => self.info_vault(transaction, name, span)?,
            InfoSubject::Recipients(target) => self.info_recipients(transaction, target, span)?,
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
    /// One graph, and the tables that belong to it.
    ///
    /// An empty list is a legitimate answer and is what a graph declared a
    /// moment ago reports: the graph exists, and reporting it as absent would
    /// make the first thing anyone does after declaring one look like a failure.
    /// A graph that does not exist is the different case, and refuses.
    ///
    /// Membership is read by filtering the database's tables rather than from a
    /// list held on the graph, because the membership already lives on the table
    /// — a second copy on the graph would be a fact able to disagree with
    /// itself, which is the same reason a bucket's chunk table is derived from
    /// its name rather than stored beside it.
    fn info_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: name.text.clone(),
                span,
            })?;
        let readable = self.readable_in(transaction)?;
        let mut names = Vec::new();
        for table in Catalog::new(transaction).tables_in(context.namespace, context.database)? {
            if table.graph != Some(id) || !nameable(&table.name) {
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
        // The edge kinds are listed beside the node tables rather than folded in
        // with them, because they are not tables: nothing can select from one,
        // and a report that mixed the two would invite a caller to try.
        let kinds: Vec<_> = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)?
            .into_iter()
            .filter(|kind| kind.graph == id)
            .map(|kind| kind.name)
            .collect();
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("tables".to_owned(), by_name(names)),
            ("edges".to_owned(), by_name(kinds)),
        ]))
    }

    /// `INFO FOR VECTOR embeddings` — its width, its distance, and its recall.
    ///
    /// The third field is the reason this is not `INFO FOR TABLE`. A vector
    /// index answers approximately, so the only number that says whether its
    /// answers are worth having is the recall it was **measured** at — and that
    /// is a property of a measurement rather than of a declaration, which is why
    /// no table report could ever carry it.
    ///
    /// It reads `none` until the index is **built**, which is where the whole
    /// graph and every stored vector are in hand at once and the measurement is
    /// therefore free of extra reads. A store declared and filled but never
    /// rebuilt reports `none`, and that is the honest answer rather than a gap:
    /// a figure derived from the build parameters would be a number nobody
    /// checked, wearing the name of one somebody did.
    ///
    /// When there is a figure it never appears alone. It carries the `k` it was
    /// measured at, how many queries it averaged, **how many records the store
    /// held at the time**, and the engine constants in force — because recall
    /// decays as records are added after a build, so a bare percentage goes
    /// stale in silence, which is the same failure by the other door.
    fn info_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vector store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        let TableKind::Vector(declared) = definition.kind else {
            return Err(missing());
        };
        // The store's index, found by the property that makes it one rather than
        // by the name the desugaring gave it — a name is a spelling and this is
        // the thing itself.
        let index = Catalog::new(transaction)
            .indexes_on(id)?
            .into_iter()
            .find(|index| index.vector.is_some());
        let measured = match index {
            Some(index) => transaction.vector_recall(&index)?,
            None => None,
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            (
                "dimension".to_owned(),
                Value::Number(Number::Integer(i64::from(declared.dimension))),
            ),
            ("distance".to_owned(), Value::from(declared.distance.name())),
            // `None` and not a zero. A recall of zero is a measurement saying
            // the index finds nothing; absence says nobody has asked. Reporting
            // the second as the first is the failure this field exists to avoid.
            ("recall".to_owned(), measured.map_or(Value::None, reported)),
        ]))
    }

    /// `INFO FOR GEO places` — the store's field and its index.
    ///
    /// Shorter than [`Session::info_vector`] by exactly what the two engines
    /// differ by, and it stops being shorter than that. A vector store declares
    /// a width and a distance, so `INFO` reports both; a geo store declares
    /// nothing, so there is no parameter here to report.
    ///
    /// **But there is a measurement, and this once said there was not.** The
    /// earlier reasoning — a spatial index answers exactly, so nothing needs
    /// measuring — confused two different things. The *answer* is exact, because
    /// the predicate re-tests the real geometry above the index. The *filter* is
    /// not: it works on bounding boxes, and a box is not a geometry. How much it
    /// offers against how much survives is the health of the whole arrangement,
    /// and it is the one number that makes a structurally awkward row — a river,
    /// a road, a border, whose box is many times its own area — visible at all.
    /// `INFO FOR VAULT team` — which fields are sealed, and nothing more.
    ///
    /// The answer names each declared field and says whether it is `SECRET`. It
    /// does **not** carry a length, a fingerprint, a key identifier or a record
    /// count for the sealed ones, and that is a line rather than an omission: a
    /// length is an oracle that answers slowly, a key identifier tells an
    /// attacker which records share a key, and a reader would have no way to
    /// tell any of them was a disclosure.
    ///
    /// It does not report whether the store is sealed either. That is a property
    /// of this *process*, not of this vault, and answering it here would make a
    /// per-vault question out of a store-wide one.
    /// `INFO FOR RECIPIENTS OF team:github` — who may one day open this record.
    ///
    /// **Nothing here is filtered**, which is what keeps it safe to answer at
    /// all: the caller either holds the grant on the vault and sees the whole
    /// set, or is refused before this runs. A listing narrowed per caller would
    /// disclose by its size what it withheld by its contents, and this one has
    /// no size to read anything from.
    ///
    /// The material comes back with the names because it is the application's
    /// own ciphertext and the store never read it. What the store's own entry
    /// holds is not in the answer — see `tessari_storage::recipients`.
    fn info_recipients(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let (_, definition, held) = self.vault_record(transaction, target, span)?;
        let entries = tessari_storage::recipients(&held, &definition.name)?;
        Ok(BTreeMap::from([(
            "recipients".to_owned(),
            Value::Object(entries),
        )]))
    }

    /// `INFO FOR AUDIT` — every recorded vault read, oldest first.
    ///
    /// `BY 'ada'` narrows it to one actor, which is the shape the question is
    /// actually asked in: *this credential was compromised; what did it open,
    /// and what has to be rotated now?* Until this the answer existed only in
    /// Rust, so the operator holding that question at three in the morning had
    /// to write a program to ask it.
    ///
    /// # It takes no transaction, and that is the point
    ///
    /// The trail is written in a transaction of its own so that a `REVEAL`
    /// inside a cancelled transaction cannot roll away the record of itself.
    /// Reading it inside the caller's transaction would undo half of that: a
    /// reader would see their own uncommitted writes against the trail, and the
    /// trail is not something a caller writes to.
    ///
    /// # There is no check here, and that is not an omission
    ///
    /// The demand is declared once, where every statement's demand is declared,
    /// and it is `govern` over the store itself. A second check written here
    /// would be a second evaluator of one rule — the thing `INFO FOR ACCESS`
    /// exists as a counter-example to.
    fn info_audit(&self, actor: Option<&Name>) -> Result<BTreeMap<String, Value>> {
        let entries = match actor {
            Some(name) => tessari_storage::reads_by(self.store, &name.text)?,
            None => tessari_storage::audit_entries(self.store)?,
        };
        Ok(BTreeMap::from([(
            "audit".to_owned(),
            Value::Array(entries),
        )]))
    }

    fn info_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }
        let fields = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .map(|field| {
                (
                    field.name,
                    Value::Object(BTreeMap::from([
                        ("type".to_owned(), Value::from(field.kind.name().as_ref())),
                        ("secret".to_owned(), Value::Bool(field.secret)),
                    ])),
                )
            })
            .collect();
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("fields".to_owned(), Value::Object(fields)),
        ]))
    }

    fn info_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "geo store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if definition.kind != TableKind::Geo {
            return Err(missing());
        }
        // Found by the property that makes it the store's index rather than by
        // the name the desugaring gave it, for the reason the vector store gives:
        // a name is a spelling, and this is the thing itself.
        let index = Catalog::new(transaction)
            .indexes_on(id)?
            .into_iter()
            .find(|index| index.spatial);
        let measured = match &index {
            Some(index) => transaction.spatial_refinement(index)?,
            None => None,
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("field".to_owned(), Value::from(GEO_FIELD)),
            (
                "index".to_owned(),
                index.map_or(Value::None, |index| Value::from(index.name.as_str())),
            ),
            // `None` and not a zero, for the reason the vector store's recall is:
            // a ratio of zero would read as a filter that admits nothing, where
            // absence says nobody measured. Absence also covers a store whose
            // records never reach one another, which has no refinement cost to
            // report rather than a perfect one.
            (
                "refinement".to_owned(),
                measured.map_or(Value::None, refining),
            ),
        ]))
    }

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
        // The one read that may name a view: describing one is the point of
        // asking, and a report refused because the subject is a view would be a
        // report nobody could get for the thing they asked about.
        let (_, id) = self.resolve_any_table(transaction, table)?;
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
        let declared = (fields.len(), indexes.len());
        fields.retain(|field| readable_field(&visible, &field.name));
        indexes.retain(|index| readable_index(&visible, index));
        let whole = declared == (fields.len(), indexes.len());
        let mut report = shape_of(&definition);
        report.insert(
            "fields".to_owned(),
            Value::Array(fields.iter().map(described_field).collect()),
        );
        report.insert(
            "indexes".to_owned(),
            Value::Array(indexes.iter().map(described_index).collect()),
        );
        let (key, held) = match describe::declaration(&definition, &fields, &indexes) {
            // A narrowed view gets no script. The report above is already the
            // subset this caller may read, and that is a truthful *description*;
            // a **declaration** built from the same subset is not, because it
            // claims to re-create the table and would re-create a different one.
            // Handing it over would also disclose through the definition exactly
            // what the field grant removes from every read they make.
            Ok(_) if !whole => (
                "undefinable",
                "fields or indexes of this table are hidden from this caller".to_owned(),
            ),
            Ok(script) => ("definition", script),
            Err(unwritable) => ("undefinable", unwritable.part),
        };
        report.insert(key.to_owned(), Value::from(held.as_str()));
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
        // The same boundary the listing draws, drawn again here. A caller who
        // cannot be *shown* somebody in a list has no business reading their
        // role, their tenancy and every grant they hold by naming them instead
        // — and the singular form is the one an operator reaches for when they
        // already have a name to try.
        if !self.administers(&user) {
            return Err(Error::NotYours {
                user: user.name.clone(),
                span,
            });
        }
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
        report.insert(
            "authorities".to_owned(),
            described_authorities(&catalog, &user)?,
        );
        Ok(report)
    }

    /// Every user of the tenancy this caller administers.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, which is why
    /// there is no permission check in the body. What the body does instead is
    /// bound the answer to the caller's **own** tenancy: passing the check says
    /// somebody administers something, and it does not say they administer the
    /// whole store.
    ///
    /// The three cases are the three tenancies a user can hold, and the rule is
    /// containment rather than equality — a store owner sees everyone, a
    /// namespace owner sees that namespace, a database owner sees that database.
    /// A user of a *different* tenancy at the same depth is not visible to
    /// either, which is the case that would otherwise leak quietly.
    ///
    /// Grants are deliberately absent: they are per-user detail, one catalog read
    /// each, and `INFO FOR USER <name>` is where a single subject is examined.
    fn info_users(&self, transaction: &mut Transaction<'_>) -> Result<BTreeMap<String, Value>> {
        let catalog = Catalog::new(transaction);
        let mut listed = Vec::new();
        for user in catalog.users()? {
            if !self.administers(&user) {
                continue;
            }
            let mut described = described_user(&user);
            if let Some(id) = user.namespace
                && let Some(found) = catalog.namespace(id)?
            {
                described.insert("namespace".to_owned(), Value::from(found.name.as_str()));
            }
            if let Some(id) = user.database
                && let Some(found) = catalog.database(id)?
            {
                described.insert("database".to_owned(), Value::from(found.name.as_str()));
            }
            listed.push(Value::Object(described));
        }
        Ok(BTreeMap::from([("users".to_owned(), Value::Array(listed))]))
    }

    /// Who can reach one table, answered by asking rather than by deriving.
    ///
    /// # Why this does not read a grant
    ///
    /// Because a report that read grants and authorities and worked out what
    /// they add up to would be a **second evaluator**, and the store already has
    /// one — `Session::authorize`, the function every statement passes through.
    /// Two evaluators of one rule agree until they do not, and the moment they
    /// stop is invisible: nothing fails, the report simply becomes fiction, and
    /// the person reading it is by definition somebody auditing a system they
    /// cannot otherwise see into. So each answer here is obtained by signing a
    /// throwaway session in as that user and putting a real `SELECT` and a real
    /// `DELETE` to the real check.
    ///
    /// That also means every rule holds without being restated: the tenancy
    /// gate, the held set, the grant loop and the open-store rule all apply
    /// because they are the same code. A user declared in another namespace
    /// reports `false` for the reason they would be refused, not because this
    /// function remembered to exclude them.
    ///
    /// # Everybody administered is listed, including the ones who cannot
    ///
    /// A row saying `bob` reaches nothing looks like noise until you notice that
    /// leaving it out makes two different facts look identical — *bob cannot
    /// reach this* and *the caller cannot see bob*. An audit answer has to
    /// distinguish those, and only the caller's own tenancy boundary decides who
    /// appears, exactly as it does for `INFO FOR USERS`.
    ///
    /// The probes never run. `authorize` decides from the statement's shape, so
    /// the record id below names nothing that has to exist.
    fn info_access(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        // Resolved first, so a report is never produced for a table that is not
        // there: an empty access list is the same shape as a typo.
        let (_, id) = self.resolve_table(transaction, table)?;
        let catalog = Catalog::new(transaction);
        let Some(definition) = catalog.table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        let users = catalog.users()?;
        // Three statements and not two. Reaching a table starts with selecting
        // the container it is in, and that first step is where a declared
        // tenancy is enforced — `within_tenancy` looks at `USE` and at nothing
        // else, because a selection is a name until something resolves it. A
        // probe handed the asker's selection outright would therefore skip the
        // only gate confining a tenant, and report that somebody from another
        // namespace reads this table. The first draft of this function did
        // exactly that, and the cross-product test caught it.
        let selecting = selecting(
            self.namespace(),
            table
                .database
                .as_ref()
                .map_or_else(|| self.database(), |name| Some(name.text.as_str())),
            table.span,
        );
        let reading = reading(table);
        let writing = writing(table);
        let mut listed = Vec::new();
        for user in users {
            if !self.administers(&user) {
                continue;
            }
            let mut probe = self.probing(user.id)?;
            let arrived = probe.authorize(self.store, &selecting, span).is_ok();
            let read = arrived && probe.authorize(self.store, &reading, span).is_ok();
            let write = arrived && probe.authorize(self.store, &writing, span).is_ok();
            listed.push(Value::Object(BTreeMap::from([
                ("user".to_owned(), Value::from(user.name.as_str())),
                ("read".to_owned(), Value::Bool(read)),
                ("write".to_owned(), Value::Bool(write)),
            ])));
        }
        Ok(BTreeMap::from([
            ("table".to_owned(), Value::from(definition.name.as_str())),
            ("access".to_owned(), Value::Array(listed)),
        ]))
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

    /// One consumer: what was declared, what this process is doing with it, and
    /// what it does not promise.
    ///
    /// Three named groups rather than one flat object, for `INFO FOR NODE`'s
    /// reason plus one of its own:
    ///
    /// - `declared` is what a backup carries and what every node agrees on;
    /// - `running` is this process only, and is empty on a node that has not
    ///   started it;
    /// - `guarantees` is here because the loudest failure of systems that ship
    ///   this feature is not a bug, it is that their delivery semantics are
    ///   documented somewhere other than where a person configures the thing.
    ///   Somebody reading this output is configuring it right now.
    fn info_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let found = Catalog::new(transaction)
            .consumers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(consumer) = found else {
            return Err(Error::Unknown {
                entity: "consumer",
                name: name.text.clone(),
                span,
            });
        };
        let destination = self.named_table(transaction, &consumer)?;
        Ok(BTreeMap::from([
            (
                "declared".to_owned(),
                described_consumer(&consumer, &destination),
            ),
            (
                "running".to_owned(),
                running_state(self.store.running().progress(&consumer.name).as_ref()),
            ),
            ("guarantees".to_owned(), guarantees()),
        ]))
    }

    /// Every declared consumer, with whether this process is running it.
    ///
    /// The counters are left to `INFO FOR CONSUMER <name>`: this is the listing
    /// an operator reads to find out *which* consumer to ask about, and a table
    /// of every partition position would bury that.
    fn info_consumers(&self, transaction: &mut Transaction<'_>) -> Result<BTreeMap<String, Value>> {
        let declared = Catalog::new(transaction).consumers()?;
        let mut described = Vec::with_capacity(declared.len());
        for consumer in declared {
            let running = self.store.running().progress(&consumer.name).is_some();
            described.push(Value::Object(BTreeMap::from([
                ("name".to_owned(), Value::from(consumer.name.as_str())),
                ("topic".to_owned(), Value::from(consumer.topic.as_str())),
                ("group".to_owned(), Value::from(consumer.group.as_str())),
                ("running".to_owned(), Value::Bool(running)),
            ])));
        }
        Ok(BTreeMap::from([(
            "consumers".to_owned(),
            Value::Array(described),
        )]))
    }

    /// The destination, written the way a statement would name it.
    ///
    /// Resolved back from ids rather than stored as text, so a table renamed
    /// under a consumer reports its new name instead of the one that was typed.
    fn named_table(
        &self,
        transaction: &mut Transaction<'_>,
        consumer: &ConsumerDefinition,
    ) -> Result<String> {
        let named = Catalog::new(transaction)
            .tables_in(consumer.namespace, consumer.database)?
            .into_iter()
            .find(|table| table.id == consumer.destination)
            .map(|table| table.name);
        // A destination that has been dropped is reported as gone rather than
        // omitted: a consumer writing into nothing is the condition an operator
        // is looking for, and a missing field reads as a display bug.
        Ok(named.unwrap_or_else(|| "<dropped>".to_owned()))
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
        after: None,
        approximate: None,
        start: None,
        limit: None,
        using: None,
        timeout: None,
        version: None,
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
    ]);
    // Present only on a view, and it carries the read rather than a flag. A
    // marker alone would say the least useful true thing: two views differ
    // entirely in what they answer and not at all in being views, so a report
    // omitting the read describes every view identically. It is the same reason
    // the endpoint pair below is reported and not merely the edge flag.
    if let Some(read) = definition.view_read() {
        shape.insert("view".to_owned(), Value::from(read));
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
