//! A table's definition and the shape it declares.

use super::ShardMap;
use super::{
    EdgeDeclaration, FIELD_BUCKET, FIELD_CEILING, FIELD_COLLECTION, FIELD_CONFLICT, FIELD_DATABASE,
    FIELD_EDGE, FIELD_ENDPOINTS, FIELD_GEO, FIELD_GRAPH, FIELD_ID, FIELD_IDENTITY, FIELD_NAME,
    FIELD_NAMESPACE, FIELD_PARTITION, FIELD_QUEUE, FIELD_SCHEMAFULL, FIELD_SERIES, FIELD_SHARDS,
    FIELD_SPACE, FIELD_TOPIC, FIELD_VAULT, FIELD_VECTOR, FIELD_VIEW, QueueDeclaration,
    SeriesDeclaration, StoredKind, TableKind, VaultCustody, VaultDeclaration, VectorDeclaration,
    ViewDeclaration, byte_count, ceiling, field_id, field_name, flag, identity_kind, number,
    object,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{
    ConflictPolicy, DatabaseId, GraphId, IdentityKind, NamespaceId, RecordId, TableId, Value,
};

/// A table within a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDefinition {
    /// The table's id.
    pub id: TableId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// Its name, unique within that database.
    pub name: String,
    /// Whether the table refuses a field it has no definition for.
    ///
    /// False is the schemaless default: declared fields are constrained and
    /// anything else passes. True is what turns a set of field declarations into
    /// a schema, because the mistake worth catching — a misspelled field name —
    /// writes a field nobody declared.
    pub schemafull: bool,
    /// Which engine's rules the table plays by.
    ///
    /// One kind rather than the three independent booleans this replaces. The
    /// booleans admitted eight states of which four meant anything, and nothing
    /// in the catalog refused the other four — only the grammar did, because
    /// each was reached by a different statement. That put the invariant in the
    /// parser and left the type able to describe a table that is both a bucket
    /// and an edge (G021 node D, criterion C1).
    pub kind: TableKind,
    /// What the table names a record with when the caller does not.
    ///
    /// A record written before this field existed reads [`IdentityKind::Int`],
    /// which is what such a table would have used had the choice existed — every
    /// generated identity before this was an `INSERT`'s UUID, and no table had a
    /// declaration to contradict. So no stored table is touched and no migration
    /// step is owed, the same contract the flags carry.
    ///
    /// An **unrecognised** word is a different case and is refused: a table
    /// written by a later build under a scheme this one does not know must not be
    /// read as though it used the one this build prefers, because the two would
    /// then name records into one table on two schemes.
    pub identity: IdentityKind,
    /// The graph this table belongs to, when it belongs to one.
    ///
    /// `None` is every table that existed before graphs did, and is what an
    /// entry written without the field decodes to — the same additive contract
    /// the flags and `identity` keep. Membership is a **clause** rather than a
    /// word (`DEFINE TABLE person IN social`) because a node kind is a table in
    /// every respect that matters — selected from, inserted into, indexed,
    /// granted on — and differs by exactly this one fact (Q-314).
    pub graph: Option<GraphId>,
    /// What the table does with a write it cannot order (G027 S3.2).
    ///
    /// `None` is **never stated** and reads as [`ConflictPolicy::Refuse`], which
    /// is what ADR-0075 has every table do and what every table that existed
    /// before the clause did has always had done for it — so nothing on disk is
    /// rewritten and no migration step is owed.
    ///
    /// On the **table** rather than the namespace, where the replication class
    /// went, because the two answer different questions at the levels they
    /// belong to: a namespace says whether a second writer may exist at all, a
    /// table says what to do when two of them have written one record without
    /// seeing each other. A counter can tolerate a dropped update beside a
    /// ledger row in the same namespace that cannot (Q-633).
    pub conflict: Option<ConflictPolicy>,
    /// Where the table's shards begin, when it is split (G031, ADR-0080).
    ///
    /// `None` is a table that is not split, which is every table that existed
    /// before the clause did — written only when present, so no stored entry is
    /// touched and no migration step is owed, the contract every optional field
    /// here keeps.
    pub shards: Option<ShardMap>,
    /// The field whose value leads every record's identity, when the table was
    /// declared `PARTITION BY` it (ADR-0096); written only when present.
    pub partition: Option<String>,
}

impl TableDefinition {
    /// Whether the table holds edges, declared pair or not.
    #[must_use]
    pub fn is_edge(&self) -> bool {
        matches!(self.kind, TableKind::Edge(_))
    }

    /// Whether the table holds files.
    #[must_use]
    pub fn is_bucket(&self) -> bool {
        matches!(self.kind, TableKind::Bucket(_))
    }

    /// The largest file this bucket accepts, in bytes, when it declared one.
    ///
    /// `None` on a bucket with no `MAX`, and on everything that is not a
    /// bucket. The two answer alike because the caller asking is a write path
    /// deciding whether to refuse, and neither has a ceiling to refuse against.
    #[must_use]
    pub fn byte_ceiling(&self) -> Option<u64> {
        match self.kind {
            TableKind::Bucket(max) => max,
            _ => None,
        }
    }

    /// Whether this store holds secrets.
    ///
    /// The check every generic path asks before it does anything with these
    /// records. It is a method on the definition rather than a rule written
    /// down somewhere, because the survey that preceded this feature found that
    /// **no** generic read path in this store consults the table kind at all —
    /// so each refusal is a place that had to be given the question, and a
    /// The read this table stands for, when it is a view.
    ///
    /// `None` for every other kind, which is what lets a caller ask the question
    /// without first asking what kind it is.
    #[must_use]
    pub fn view_read(&self) -> Option<&str> {
        match &self.kind {
            TableKind::View(declared) => Some(declared.read.as_str()),
            _ => None,
        }
    }

    /// question spelled the same way everywhere is one a reviewer can find.
    #[must_use]
    pub fn is_vault(&self) -> bool {
        matches!(self.kind, TableKind::Vault(_))
    }

    /// Whether this table is a queue.
    ///
    /// Asked for the same reason [`Self::is_vault`] is asked: three of a
    /// queue's fields are written by the engine after a caller's record has been
    /// validated, so strictness has to know the kind before it can excuse them.
    #[must_use]
    pub fn is_queue(&self) -> bool {
        matches!(self.kind, TableKind::Queue(_))
    }

    /// The vault's key and what it is sealed under (ADR-0093).
    ///
    /// `None` for everything that is not a vault, which is the same answer a
    /// vault gives if its declaration were ever absent — and that second case
    /// cannot arise, because a declaration that will not parse is refused at
    /// `from_value` rather than read as a vault with no key.
    #[must_use]
    pub fn vault_custody(&self) -> Option<&VaultCustody> {
        match &self.kind {
            TableKind::Vault(declared) => Some(&declared.custody),
            _ => None,
        }
    }

    /// Whether the declaration that created this was `DEFINE COLLECTION`.
    #[must_use]
    pub fn is_collection(&self) -> bool {
        self.kind == TableKind::Collection
    }

    /// The pair of tables the edge table joins, when it declared one.
    ///
    /// `None` covers both a table that is not an edge table at all and one
    /// declared `EDGE` with no pair, because the two answer the same question
    /// the same way: there is no endpoint to check a `RELATE` against. A caller
    /// that needs to tell them apart asks [`is_edge`](Self::is_edge) first, and
    /// exactly one place does.
    #[must_use]
    pub fn edge_endpoints(&self) -> Option<&EdgeDeclaration> {
        match &self.kind {
            TableKind::Edge(declared) => declared.as_ref(),
            _ => None,
        }
    }
}

impl TableDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_SCHEMAFULL.to_owned(), Value::Bool(self.schemafull)),
            // Four named flags on disk. The kind is how this build talks about a
            // table, not a change to how one is stored, so no catalog entry is
            // touched, no migration step is owed, and a build without the kind
            // reads everything this one writes.
            (
                FIELD_EDGE.to_owned(),
                Value::Bool(matches!(self.kind, TableKind::Edge(_))),
            ),
            (
                FIELD_BUCKET.to_owned(),
                Value::Bool(matches!(self.kind, TableKind::Bucket(_))),
            ),
            (
                FIELD_COLLECTION.to_owned(),
                Value::Bool(self.kind == TableKind::Collection),
            ),
            (
                FIELD_GEO.to_owned(),
                Value::Bool(self.kind == TableKind::Geo),
            ),
            (FIELD_IDENTITY.to_owned(), Value::from(self.identity.name())),
        ]);
        // Written only when there is one, for the reason the endpoint pair is:
        // a membership nobody declared is absent rather than zero, and zero is
        // a graph id the allocator can legitimately never hand out but which a
        // future reader would have to know that about.
        if let Some(graph) = self.graph {
            fields.insert(FIELD_GRAPH.to_owned(), number(graph.get()));
        }
        // Written only when the operator said something, for the reason the
        // graph membership above is: silence and a declared refusal are
        // different facts, and a policy word written for every table would make
        // them the same one.
        if let Some(conflict) = self.conflict {
            fields.insert(FIELD_CONFLICT.to_owned(), conflict.to_value());
        }
        // Written only when the table is split, for the same reason: an
        // unsharded table's entry stays byte-for-byte what it was.
        if let Some(shards) = &self.shards {
            fields.insert(FIELD_SHARDS.to_owned(), shards.to_value());
        }
        if let Some(partition) = &self.partition {
            fields.insert(FIELD_PARTITION.to_owned(), Value::from(partition.as_str()));
        }
        // A declaration is not a flag, so it is written only by the edge table
        // that has one. Absent is how every edge table declared without a pair
        // reads, which is the same compatibility contract the flags keep: the
        // clause is optional, so an entry written before it existed decodes as
        // the permissive edge table it is.
        if let TableKind::Edge(Some(endpoints)) = &self.kind {
            fields.insert(FIELD_ENDPOINTS.to_owned(), endpoints.to_value());
        }
        // The vector store is the one kind carried by its declaration rather
        // than by a flag beside it, because it is the one kind with nothing to
        // say when the declaration is absent: an edge table without endpoints is
        // still an edge table, while a vector store without a width and a
        // distance is not a vector store at all. Presence is therefore the whole
        // statement, and a fourth flag would be a second place holding one fact.
        if let TableKind::Vector(declared) = &self.kind {
            fields.insert(FIELD_VECTOR.to_owned(), declared.to_value());
        }
        // A vault is carried by its declaration for the reason a vector store
        // is, and with more riding on it: presence is the whole statement,
        // because a vault without its key is not a vault with a missing
        // property — it is a store whose records are permanently unreadable.
        // The downgrade case, stated because it is easy to assume the opposite:
        // a build that predates vaults sets no flag, finds no vector, and reads
        // this entry as a plain **table**. It would then let `SELECT` return the
        // records. What it returns is ciphertext — the plaintext is not in the
        // store to be served — so the secrets hold, but every refusal the word
        // carries is gone. Opening a store with an older binary is therefore a
        // real downgrade and not merely a loss of the word (Q-412).
        if let TableKind::Vault(declared) = &self.kind {
            fields.insert(FIELD_VAULT.to_owned(), declared.to_value());
        }
        // Written only by the bucket that declared one, on the endpoint pair's
        // contract rather than the flags': a ceiling nobody declared is absent
        // rather than zero, and zero is the one value that would have to mean
        // "unbounded" while reading as "accepts nothing".
        if let TableKind::Bucket(Some(max)) = self.kind {
            fields.insert(FIELD_CEILING.to_owned(), byte_count(max));
        }
        // A queue is carried by its declaration for the reason a vector store
        // and a vault are: a queue with no timeout is not a queue with a missing
        // property, it is a table whose holds would never lapse. The downgrade
        // case is milder than the vault's and is still worth stating: a build
        // that predates queues finds no flag and no declaration and reads this
        // entry as a plain **table**, so the records are readable, the claim
        // fields are ordinary fields, and every refusal the word carries is
        // gone — the same shape of loss, without the confidentiality.
        if let TableKind::Queue(declared) = &self.kind {
            fields.insert(FIELD_QUEUE.to_owned(), declared.to_value());
        }
        // A view is carried by its read for the reason a queue is carried by its
        // timeout. Its downgrade case is the **sharpest of the three** and is
        // worth stating plainly: a build that predates views finds no flag and
        // no declaration and reads this entry as a plain table — one whose
        // keyspace is empty. So `SELECT` answers **nothing** rather than the
        // view's records, which is a wrong answer and not a lost refusal, and
        // `CREATE` succeeds and writes records into a prefix this build will
        // never read. Opening a store holding views with an older binary is a
        // downgrade with data consequences.
        if let TableKind::View(declared) = &self.kind {
            fields.insert(FIELD_VIEW.to_owned(), declared.to_value());
        }
        // A series is carried by its retention for the reason a queue is carried
        // by its timeout. Its downgrade case is the mildest of the four and is
        // still worth stating: a build that predates the kind finds no flag and
        // no declaration and reads this entry as a plain table, so every record
        // is readable — including the ones past the floor, which this build
        // hides. The loss is a refusal rather than an answer, the queue's shape
        // and not the view's.
        if let TableKind::Series(declared) = &self.kind {
            fields.insert(FIELD_SERIES.to_owned(), declared.to_value());
        }
        // A space is carried by its declaration for the reason a series is. A
        // build that predates the kind reads a plain schemaless table, which is
        // what a space was until G036, and loses only the limit.
        if let TableKind::Space(declared) = &self.kind {
            fields.insert(FIELD_SPACE.to_owned(), declared.to_value());
        }
        // A topic the same way (G037). A build that predates the kind reads a
        // plain schemaless table and would let a message be rewritten.
        if let TableKind::Topic(declared) = &self.kind {
            fields.insert(FIELD_TOPIC.to_owned(), declared.to_value());
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// An entry written before a flag existed does not carry it, and reads as
    /// `false` — which is what such a table is. Refusing it instead would make an
    /// added property unreadable rather than absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "table")?;
        Ok(Self {
            id: TableId::new(field_id(fields, FIELD_ID, "table")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "table")?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, "table")?),
            name: field_name(fields, "table")?,
            schemafull: flag(fields, FIELD_SCHEMAFULL, "table")?,
            kind: TableKind::from_parts(StoredKind {
                edge: flag(fields, FIELD_EDGE, "table")?,
                bucket: flag(fields, FIELD_BUCKET, "table")?,
                collection: flag(fields, FIELD_COLLECTION, "table")?,
                geo: flag(fields, FIELD_GEO, "table")?,
                endpoints: match fields.get(FIELD_ENDPOINTS) {
                    Some(value) => Some(EdgeDeclaration::from_value(value)?),
                    None => None,
                },
                vector: match fields.get(FIELD_VECTOR) {
                    Some(value) => Some(VectorDeclaration::from_value(value)?),
                    None => None,
                },
                vault: match fields.get(FIELD_VAULT) {
                    Some(value) => Some(VaultDeclaration::from_value(value)?),
                    None => None,
                },
                queue: match fields.get(FIELD_QUEUE) {
                    Some(value) => Some(QueueDeclaration::from_value(value)?),
                    None => None,
                },
                view: match fields.get(FIELD_VIEW) {
                    Some(value) => Some(ViewDeclaration::from_value(value)?),
                    None => None,
                },
                series: match fields.get(FIELD_SERIES) {
                    Some(value) => Some(SeriesDeclaration::from_value(value)?),
                    None => None,
                },
                space: match fields.get(FIELD_SPACE) {
                    Some(value) => {
                        Some(crate::catalog::space::SpaceDeclaration::from_value(value)?)
                    }
                    None => None,
                },
                topic: match fields.get(FIELD_TOPIC) {
                    Some(value) => {
                        Some(crate::catalog::topic::TopicDeclaration::from_value(value)?)
                    }
                    None => None,
                },
                ceiling: ceiling(fields)?,
            })?,
            identity: identity_kind(fields, "table")?,
            graph: match fields.get(FIELD_GRAPH) {
                Some(_) => Some(GraphId::new(field_id(fields, FIELD_GRAPH, "table")?)),
                None => None,
            },
            conflict: match fields.get(FIELD_CONFLICT) {
                None => None,
                Some(held) => Some(ConflictPolicy::from_value(held).ok_or(
                    Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_CONFLICT,
                        found: "a conflict policy this build does not have",
                    },
                )?),
            },
            shards: match fields.get(FIELD_SHARDS) {
                None => None,
                Some(held) => Some(ShardMap::from_value(held)?),
            },
            partition: match fields.get(FIELD_PARTITION) {
                None => None,
                Some(Value::String(field)) => Some(field.clone()),
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_PARTITION,
                        found: "a partition field that is not a name",
                    });
                }
            },
        })
    }
}

/// What kind of table to create.
///
/// A struct rather than two `bool` parameters in a row, for the reason
/// [`tessari_types::TableId`] is a newtype rather than a `u32`: `create_table(ns,
/// db, "follows", false, true)` compiles just as well with the pair transposed,
/// and would create a schemafull table where an edge table was meant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableShape {
    /// Refuse a field the table has no declaration for.
    ///
    /// Orthogonal to the kind, and the only one of the four that was: a table
    /// and a collection can each be schemafull, while nothing can be both a
    /// bucket and an edge.
    pub schemafull: bool,
    /// Which engine's rules the table plays by.
    pub kind: TableKind,
    /// What the table names a record with when the caller does not.
    ///
    /// Carried on the shape rather than passed beside it for the reason the
    /// shape exists at all: a fifth positional argument next to four others is
    /// one transposition away from a table that mints UUIDs where a counter was
    /// meant, and nothing about the resulting store would look wrong.
    pub identity: IdentityKind,
    /// The graph the table belongs to, when the declaration named one.
    pub graph: Option<GraphId>,
    /// What the table does with a write it cannot order, when it said.
    ///
    /// Carried on the shape for the reason every other field here is: a
    /// declaration passed beside the shape is a declaration a later caller can
    /// forget to pass, and a table silently refusing writes its operator asked
    /// to be taken looks like nothing at all being wrong.
    pub conflict: Option<ConflictPolicy>,
    /// Where the table's shards begin, as the declaration wrote them.
    ///
    /// Empty is a table that is not split. Carried as written rather than as a
    /// map, because turning it into one is where the refusals live and they
    /// belong to the catalog that stores the result (`create_table`).
    pub split: Vec<RecordId>,
    /// The field whose value leads every record's identity, when the table is
    /// partitioned by one (`PARTITION BY region`, ADR-0096).
    pub partition: Option<String>,
}
