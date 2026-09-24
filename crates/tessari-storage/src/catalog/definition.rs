//! What a catalog entry holds, and how it becomes a value.
//!
//! A definition is an ordinary [`Value::Object`], encoded by the payload codec
//! like any other record. The catalog therefore needs no encoding of its own,
//! and a field added to a definition later is an object field the codec already
//! knows how to carry.
//!
//! Every definition carries its own id even though the id is also its address.
//! That is deliberate redundancy in exactly one direction: a definition read out
//! of a scan knows what it is without its caller having to remember which key it
//! came from.

use std::collections::BTreeMap;

use tessari_types::{
    ConflictPolicy, DatabaseId, Duration, GraphId, IdentityKind, IndexId, NamespaceId, Number,
    Path, RecordId, Replication, ReplicationClass, TableId, Value,
};

use tessari_vault::{KeyId, Wrapped};

use super::ShardMap;
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_FIELDS: &str = "fields";
const FIELD_UNIQUE: &str = "unique";
const FIELD_SEARCH: &str = "search";
const FIELD_VECTOR: &str = "vector";
const FIELD_SPATIAL: &str = "spatial";
const FIELD_SCHEMAFULL: &str = "schemafull";
const FIELD_EDGE: &str = "edge";
const FIELD_BUCKET: &str = "bucket";
const FIELD_COLLECTION: &str = "collection";
const FIELD_GEO: &str = "geo";
const FIELD_VAULT: &str = "vault";
const FIELD_KEY_ID: &str = "key_id";
const FIELD_WRAPPED: &str = "wrapped";
const FIELD_ENDPOINTS: &str = "endpoints";
const FIELD_FROM: &str = "from";
const FIELD_TO: &str = "to";
const FIELD_ORDER: &str = "order";
const FIELD_DESCENDING: &str = "descending";
const FIELD_IDENTITY: &str = "identity";
const FIELD_GRAPH: &str = "graph";
const FIELD_CEILING: &str = "ceiling";
const FIELD_QUEUE: &str = "queue";
const FIELD_VIEW: &str = "view";
const FIELD_READ: &str = "read";
const FIELD_TIMEOUT: &str = "timeout";
const FIELD_ATTEMPTS: &str = "attempts";
const FIELD_SERIES: &str = "series";
const FIELD_RETAIN: &str = "retain";
const FIELD_DIMENSION: &str = "dimension";
const FIELD_DISTANCE: &str = "distance";
const FIELD_REPLICATION: &str = "replication";
const FIELD_REPLICATION_CLASS: &str = "replication_class";
const FIELD_CONFLICT: &str = "conflict";
const FIELD_SHARDS: &str = "shards";

/// A namespace: the outermost tenancy level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceDefinition {
    /// The namespace's id.
    pub id: NamespaceId,
    /// Its name, which may change without moving anything.
    pub name: String,
    /// How many copies the cluster is asked to keep of it (ADR-0060).
    ///
    /// `None` is **never stated**, and every namespace in every store that
    /// existed before this field did reads that way — which is the correct
    /// reading and not a fallback, exactly as an absent epoch flag is epoch
    /// zero. It is deliberately not [`Replication::None`]: a namespace that
    /// declined replication is honoured, and a namespace nobody ever asked is
    /// refused at the moment a second node would hold it. That difference is
    /// unrecoverable once namespaces exist, which is why the field lands before
    /// the cluster does rather than with it.
    ///
    /// The counter-argument is on the record and it is `replica.rs`'s own: a
    /// field nothing reads is a decision taken with no way to find out it was
    /// wrong. It applies to a copy count held on a **replica**, where the value
    /// can be derived later from the peers that exist. It does not apply here,
    /// because what is bought is not the number — it is the distinction between
    /// silence and a stated answer, and silence cannot be reconstructed.
    pub replication: Option<Replication>,
    /// How many writers it admits — single-leader, or multi-master (G027 S2.1).
    ///
    /// `None` is **never stated** and reads as single-leader wherever it is
    /// asked, which is what every namespace written before this field did and
    /// what the engine has always enforced. It is kept apart from a stated
    /// [`ReplicationClass::SingleLeader`] for the reason its neighbour keeps
    /// its own two absences apart: an operator who considered the question and
    /// answered it has told the cluster something, and `INFO FOR` reports a
    /// decision differently from a silence.
    ///
    /// Separate from `replication` rather than folded into it because the two
    /// are independent — how many copies and how many writers — and a namespace
    /// declared multi-master still has a factor.
    pub class: Option<ReplicationClass>,
}

/// A database within a namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDefinition {
    /// The database's id.
    pub id: DatabaseId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// Its name, unique within that namespace.
    pub name: String,
}

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

    /// The vault's key, sealed under the store's master key.
    ///
    /// `None` for everything that is not a vault, which is the same answer a
    /// vault gives if its declaration were ever absent — and that second case
    /// cannot arise, because a declaration that will not parse is refused at
    /// `from_value` rather than read as a vault with no key.
    #[must_use]
    pub fn vault_key(&self) -> Option<&Wrapped> {
        match &self.kind {
            TableKind::Vault(declared) => Some(&declared.key),
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

impl NamespaceDefinition {
    /// The value written to the catalog.
    ///
    /// The replication field is written **only when it was stated**, so a
    /// namespace that said nothing encodes to the exact object it encoded to
    /// before this field existed. Nothing already stored is rewritten, and the
    /// absence carries the meaning instead of a placeholder standing in for it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]);
        if let Some(replication) = self.replication {
            fields.insert(FIELD_REPLICATION.to_owned(), replication.to_value());
        }
        if let Some(class) = self.class {
            fields.insert(FIELD_REPLICATION_CLASS.to_owned(), class.to_value());
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type — including a replication policy this build does not
    /// recognise, which refuses rather than reading as never-stated. A
    /// namespace written by a later build under a placement policy must not be
    /// served here as though nobody had ever declared one.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "namespace")?;
        let replication = match fields.get(FIELD_REPLICATION) {
            None => None,
            Some(held) => Some(
                Replication::from_value(held).ok_or(Error::CatalogMalformed {
                    entity: "namespace",
                    field: FIELD_REPLICATION,
                    found: "a replication policy this build does not have",
                })?,
            ),
        };
        let class = match fields.get(FIELD_REPLICATION_CLASS) {
            None => None,
            Some(held) => Some(ReplicationClass::from_value(held).ok_or(
                Error::CatalogMalformed {
                    entity: "namespace",
                    field: FIELD_REPLICATION_CLASS,
                    found: "a replication class this build does not have",
                },
            )?),
        };
        Ok(Self {
            id: NamespaceId::new(field_id(fields, FIELD_ID, "namespace")?),
            name: field_name(fields, "namespace")?,
            replication,
            class,
        })
    }
}

impl DatabaseDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "database")?;
        Ok(Self {
            id: DatabaseId::new(field_id(fields, FIELD_ID, "database")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "database")?),
            name: field_name(fields, "database")?,
        })
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
}

/// Which engine's rules a table plays by.
///
/// The four are exclusive by construction, which is the whole point of the type:
/// the booleans it replaces described eight states, four of them meaningless,
/// and only the grammar kept them apart because each kind is reached by a
/// different statement. A parser is the wrong place for an invariant about what
/// a stored table *is* — nothing stops a later caller building the definition by
/// hand, and a table that is both a bucket and an edge would take the bucket's
/// refusal of `CREATE` and the edge's endpoint indexes into one record with
/// nothing anywhere in an error state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TableKind {
    /// Records with named fields — `DEFINE TABLE`.
    #[default]
    Table,
    /// Records that are one value rather than named fields — `DEFINE
    /// COLLECTION`.
    ///
    /// Kept distinct from a schemaless table, which behaves alike, because
    /// `INFO FOR TABLE` must answer with the word that created the thing: a
    /// round trip emitting `DEFINE TABLE … SCHEMALESS` for a collection would
    /// re-execute happily while losing the word.
    Collection,
    /// Files: records holding metadata the store fills in from bytes it holds,
    /// with the bytes in a companion table nothing can name — `DEFINE BUCKET`.
    ///
    /// `CREATE`, `UPDATE` and `SET` against one are refused, because metadata a
    /// caller writes by hand is metadata that can lie, and a size disagreeing
    /// with the bytes is a lie nothing would ever catch. Reading is not
    /// restricted: listing a bucket is `SELECT * FROM media`, a query rather
    /// than an API call, which is the point of a bucket being a table at all
    /// (ADR-0011).
    ///
    /// The `u64` is the largest file the bucket accepts, in bytes, when one was
    /// declared. It rides **on** the kind rather than beside it for the reason
    /// [`TableKind::Edge`]'s pair does: a pair of fields would make "carries a
    /// ceiling but is not a bucket" representable, and the kind exists to
    /// abolish exactly that state. Optional inside the variant because a bucket
    /// with no ceiling is still a bucket — unlike a vector store, which without
    /// a width is not a vector store.
    ///
    /// It is a count of bytes and not a size literal because the language has
    /// no size literal: digits touching a letter are a duration whatever the
    /// letter is, so `5MB` is a duration with an unrecognised unit. The grammar
    /// therefore reads `MAX 5242880`.
    Bucket(Option<u64>),
    /// Edges: records carrying `out` and `in` record references, each with an
    /// index — `DEFINE TABLE … EDGE`.
    ///
    /// The indexes are what make traversal a range read rather than a scan,
    /// without the caller having had to know to declare them. `RELATE` checks
    /// the kind before writing, because an edge nothing can traverse to is
    /// worse than a refusal.
    ///
    /// `Some` is `DEFINE TABLE … EDGE FROM a TO b`, which refuses a link whose
    /// endpoints it does not declare; `None` is the bare `EDGE`, which accepts a
    /// link between any two records. The clause is optional so that a store
    /// discovering its shape as it goes still has a spelling for that, and so
    /// that every edge table declared before the clause existed keeps its
    /// meaning (Q-297).
    ///
    /// The declaration rides **on** the kind rather than sitting beside it in a
    /// second field, because a pair would be two places holding one fact and
    /// would make "declares a pair but is not an edge table" representable — the
    /// state this type was introduced one change earlier to abolish (C1).
    Edge(Option<EdgeDeclaration>),
    /// Vectors: records holding one vector of a declared width, with the index
    /// that searches them built by the declaration — `DEFINE VECTOR`.
    ///
    /// The fifth kind, and it is a kind rather than three separate statements
    /// for the reason [`TableKind::Collection`] is one: `INFO` must answer with
    /// the word that created the thing. A store reported as a collection with a
    /// field and an index re-executes happily and loses the fact that the three
    /// belong together — which is the whole of what the word promises, since a
    /// width with no index searches nothing, an index with no width admits a row
    /// of the wrong shape, and neither without `REQUIRED` admits a record with
    /// no vector at all.
    Vector(VectorDeclaration),
    /// Places: records holding one geometry, with the spatial index that finds
    /// them built by the declaration — `DEFINE GEO`.
    ///
    /// The sixth kind, on the same test the fifth passed: `INFO` must answer
    /// with the word that created the thing, and a store reported as a
    /// collection carrying a geometry field and a spatial index re-executes
    /// happily while losing the fact that the three belong together. A field
    /// with no index makes every place query a scan, an index with no declared
    /// field indexes nothing, and neither without `REQUIRED` admits a record
    /// with no geometry at all — which is a record a place store has no way to
    /// answer for.
    ///
    /// Unlike [`TableKind::Vector`] it carries no declaration, because it has
    /// nothing to declare. A vector store without a width and a distance is not
    /// a vector store; a geo store is complete as soon as it exists. Whether it
    /// should narrow the shape it holds is decided against it (Q-324): the shape
    /// a `Closest` read needs is already enforced where that read happens, and a
    /// store narrowed to points could not express a table of regions — which
    /// `records_in_region` serves correctly today.
    Geo,
    /// Secrets: records whose declared `SECRET` fields are stored sealed, and
    /// which no generic read can reach — `DEFINE VAULT`.
    ///
    /// The seventh kind, and the first whose reason is not "`INFO` must answer
    /// with the word that created the thing". That test is passed here too, but
    /// it is not why the kind exists: a vault is the one store where the
    /// *absence* of a capability is the capability. `SELECT` is refused, an
    /// index on a secret field is refused, a filter and an ordering on one are
    /// refused, and each refusal is reachable only because the kind is on the
    /// definition where every path can see it.
    ///
    /// It carries the vault's key, sealed under the store's master key, for the
    /// reason [`TableKind::Vector`] carries its declaration: a store that has
    /// one is not the same object as a store that does not, and a key sitting
    /// in a field beside the kind would make "carries a key but is not a vault"
    /// representable — which is a table whose records nothing can ever open.
    ///
    /// Dropping the definition therefore destroys the key, and destroying the
    /// key is the deletion. Every record in the vault becomes unopenable in
    /// every backup, snapshot and replica that will ever be restored — which is
    /// the only deletion claim a store like this can honestly make, since a row
    /// delete is a statement about the live table and not about the data.
    Vault(VaultDeclaration),
    /// Work waiting to be done, handed out under a hold that lapses — `DEFINE
    /// QUEUE`.
    ///
    /// The hold is not a lease and there is no lease manager, deliberately. A
    /// claim is an ordinary **write**, so it is sequenced into the log and
    /// replicated by the mechanism every other write uses; the instant it lapses
    /// is computed once by the session that takes it and **written into the
    /// record**, the same rule `time::now()` already follows so that a replica
    /// applies what was written rather than asking its own clock; and expiry is
    /// a comparison a later reader performs rather than an event anything
    /// raises. Those three together are why the queue holds no state outside the
    /// log and therefore asks nothing of a cluster that an ordinary write does
    /// not already ask.
    ///
    /// It carries its declaration for the reason [`TableKind::Vector`] does: a
    /// timeout in a field beside the kind would make "carries a timeout but is
    /// not a queue" representable, which is the state this type abolishes.
    Queue(QueueDeclaration),
    /// A name for a read, holding no records of its own — `DEFINE VIEW`.
    ///
    /// The ninth kind, and the first that is not a store at all. Every kind
    /// before it answers *what may be done to these records*; this one has no
    /// records, so what it changes is where the records come from: a statement
    /// naming a view is rewritten to carry the view's read before anything
    /// resolves a name, and the read then runs as an ordinary materialised
    /// source.
    ///
    /// That rewrite happens **before the grant check**, which is the whole of
    /// why this is a kind and not a catalog entity of its own. A grant names a
    /// [`tessari_types::TableId`], so a view outside the table namespace would
    /// need a second permission system; inside it, a view cannot shadow a table
    /// (one name reservation answers both) and a caller reading through one is
    /// checked against the tables the view actually reads.
    ///
    /// It carries its read for the reason [`TableKind::Vector`] carries its
    /// declaration: a read in a field beside the kind would make "carries a read
    /// but is not a view" representable, which is the state this type abolishes.
    View(ViewDeclaration),
    /// Records that age out — `DEFINE SERIES`.
    ///
    /// The tenth kind, and the one that makes Time an engine rather than a
    /// convention. What it adds is not a way to store an instant — every table
    /// could already do that — but a **floor**: past it a record is not in the
    /// answer, whether or not its bytes have been removed yet.
    ///
    /// The floor is a position in the key rather than a predicate over a field,
    /// and that is the whole construction. A series table's identity is
    /// [`tessari_types::IdentityKind::Uuid`], fixed by the kind, because UUID
    /// version 7 carries the millisecond in its leading six bytes big-endian —
    /// so a read under a retention does not filter, it **starts later**. An
    /// ordinary table cannot offer that: its counter identity carries no time at
    /// all, and an age rule over one of its datetime fields is re-tested per
    /// record.
    ///
    /// The comparison is performed by the reader, not raised as an event, which
    /// is the rule [`TableKind::Queue`]'s hold already follows. One consequence
    /// is worth stating where it cannot be missed: **the removal is a separate
    /// act from the hiding.** Correctness comes from the read, so a removal pass
    /// that lags, is throttled or never runs costs storage and never an answer.
    Series(SeriesDeclaration),
}

/// How long a series table answers with a record.
///
/// One field, and it is the whole capability, so it has no default for the
/// reason [`QueueDeclaration::timeout`] has none: a series table that keeps
/// everything is a table, and a retention the store guessed would drop somebody's
/// records at a boundary nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesDeclaration {
    /// How far back the answer reaches.
    ///
    /// Measured from the instant the read happens, against the millisecond the
    /// record's identity carries. Not stored on the record, unlike a queue's
    /// deadline, because there is nothing to write it to: the rule is a property
    /// of the table and applies to records written before it as well as after.
    pub retain: Duration,
}

impl SeriesDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([(
            FIELD_RETAIN.to_owned(),
            Value::Duration(self.retain),
        )]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the retention is missing or is
    /// not a duration.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "series";
        let fields = object(value, ENTITY)?;
        let Some(Value::Duration(retain)) = fields.get(FIELD_RETAIN) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_RETAIN,
                found: fields
                    .get(FIELD_RETAIN)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self { retain: *retain })
    }
}

/// The read a view names.
///
/// # Text, and not a serialised tree
///
/// The same choice a field's `DEFAULT` makes and for the reason stated there —
/// the storage layer cannot evaluate a TessariQL expression, so a definition
/// keeps the text it was written as and the layer that owns the language parses
/// it back. Two properties follow that a stored tree would not have: `INFO`
/// answers with the statement somebody typed rather than a re-rendered one that
/// happens to mean the same thing, and a view written before a clause existed
/// cannot decode into a read that silently lost it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDeclaration {
    /// The read, exactly as it was written.
    pub read: String,
}

impl ViewDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([(
            FIELD_READ.to_owned(),
            Value::from(self.read.as_str()),
        )]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the read is missing or is not a
    /// string.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "view";
        let fields = object(value, ENTITY)?;
        let Some(Value::String(read)) = fields.get(FIELD_READ) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_READ,
                found: fields
                    .get(FIELD_READ)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self { read: read.clone() })
    }
}

/// What a queue calls the instant a record's current hold lapses.
///
/// Visible and ordinary, so `SELECT` can answer *what is held and until when* —
/// which is the most common thing anybody does with a queue that has gone quiet.
/// It could instead have been hidden behind a byte no identifier can spell, the
/// way a bucket's chunk table is, and that was rejected for exactly that reason:
/// a queue whose state cannot be read is a queue nobody can debug.
///
/// The cost of being visible is that a payload field of this name collides, and
/// the collision is refused at the write naming the field rather than absorbed
/// silently (Q-461).
///
/// A lapsed record is **not** rewritten — nothing sweeps a passed deadline away
/// — so a reader asking for unclaimed records compares rather than testing for
/// absence: `claimed_until IS NONE OR claimed_until < time::now()`.
pub const QUEUE_CLAIMED_UNTIL: &str = "claimed_until";

/// What a queue calls who is holding a record.
///
/// An object of two routes — `consumer`, the name a session declared, and
/// `instance`, the value the engine minted for that session — because the pair
/// is **one fact**: who took this. Two flat fields would be two payload
/// collision surfaces where one will do, and a route into a stored object is
/// already how a condition reaches a nested value.
///
/// **Absent when the session never said who it was.** A claim from an
/// undeclared session writes nothing here, which keeps every existing caller
/// working unchanged and makes the absence mean something true — *nobody said*
/// — rather than a default that is itself a claim.
///
/// Visible and ordinary for [`QUEUE_CLAIMED_UNTIL`]'s reason, and here the
/// reason is sharper: *who has this* is the first question asked of a queue that
/// has gone quiet, and it is the one question the engine could not answer at all
/// until this field existed.
///
/// The instance is **not** derived from a sign-in ticket and never shares its
/// value. A ticket is a credential; this is read by anyone who may `SELECT` the
/// queue, and one value serving both would publish the first to everybody
/// holding the second.
pub const QUEUE_CLAIMED_BY: &str = "claimed_by";

/// A route inside [`QUEUE_CLAIMED_BY`]: the name the session declared.
pub const CLAIMED_BY_CONSUMER: &str = "consumer";

/// A route inside [`QUEUE_CLAIMED_BY`]: the value the engine minted.
pub const CLAIMED_BY_INSTANCE: &str = "instance";

/// What a queue calls the number of times a record has been handed out.
///
/// Counted at the hand-out and not at a failure, because how many times a record
/// was handed out is a fact the store can observe, while how many times the work
/// failed is a fact only the worker holds — and a count the store cannot verify
/// is a count that will eventually be wrong.
pub const QUEUE_ATTEMPTS: &str = "attempts";

/// What a vector store calls the field its vectors are in.
///
/// Fixed rather than named in the declaration, because a store whose vector
/// field could be called anything is a store every reader has to look up before
/// writing to it — and the statement already says the whole of what the field
/// is. It is the same name as the store's index, which does not collide: fields
/// and indexes are separate namespaces.
pub const VECTOR_FIELD: &str = "vector";

/// What a geo store calls the field its geometries are in.
///
/// Fixed for the reason [`VECTOR_FIELD`] is, and named after the **type** rather
/// than after the statement's word. In the vector store those coincide — the
/// word, the field and the type are all `vector` — and here they cannot, since
/// the word is `GEO` and the type is `geometry`. The rule that settles it is the
/// one the vector field states: the name is the whole of what the field is, and
/// what it is is a geometry.
pub const GEO_FIELD: &str = "geometry";

/// The key a vault's records are sealed under, sealed itself.
///
/// On the kind rather than beside it, for the reason [`EdgeDeclaration`] rides
/// on `Edge`: a field beside the kind would make "carries a key but is not a
/// vault" representable, and that state is a table full of records nothing can
/// ever open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultDeclaration {
    /// The vault's own key, sealed under the store's master key.
    ///
    /// Every record in the vault has its data key wrapped under this one, so
    /// this single value is what stands between a stolen backend and every
    /// secret the vault holds — and it is itself unreadable without a
    /// passphrase that is never stored anywhere.
    pub key: Wrapped,
}

/// How wide a vector store's vectors are, and what distance searches them.
///
/// Both are on the kind rather than beside it, for the reason
/// [`EdgeDeclaration`] rides on `Edge`: a pair of fields would make "declares a
/// width but is not a vector store" representable, and that is the state the
/// type exists to abolish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorDeclaration {
    /// How many components every vector in the store holds.
    ///
    /// A `u32` because that is how this file stores every small integer, and
    /// because the parser refuses a width no `u32` could carry — a store that
    /// could not write its own declaration back would report a width it was
    /// never given.
    pub dimension: u32,
    /// The distance its index is built and searched with.
    pub distance: VectorDistance,
}

impl VectorDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_DIMENSION.to_owned(), number(self.dimension)),
            (FIELD_DISTANCE.to_owned(), Value::from(self.distance.name())),
        ]))
    }

    /// Read a declaration back.
    ///
    /// A distance this build does not recognise is **refused**, on the same
    /// reasoning `identity_kind` records: an unknown word is a store already
    /// searched some other way, and reading it as `cosine` would answer a
    /// nearest-neighbour question from a graph built for a different geometry —
    /// plausible neighbours that are not the nearest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the width or the distance is
    /// missing, holds the wrong type, or names a distance this build has not.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "vector store";
        let fields = object(value, ENTITY)?;
        let Some(Value::String(distance)) = fields.get(FIELD_DISTANCE) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_DISTANCE,
                found: fields
                    .get(FIELD_DISTANCE)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self {
            dimension: field_id(fields, FIELD_DIMENSION, ENTITY)?,
            distance: VectorDistance::parse(distance).ok_or(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_DISTANCE,
                found: "a distance this build does not have",
            })?,
        })
    }
}

/// How long a queue holds a claim, and how many times it hands a record out.
///
/// The timeout is the whole capability, which is why it has no default: a queue
/// whose holds never lapse is a table with two extra fields, and a queue whose
/// timeout the store guessed would hand work to a second worker at a moment
/// nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDeclaration {
    /// How long a claim holds a record before it lapses.
    ///
    /// Added to the instant the claiming session reads, once, and written into
    /// the record — so the deadline in the log is a value every node agrees
    /// about rather than a computation each one repeats against its own clock.
    pub timeout: Duration,
    /// How many times one record may be handed out, when a ceiling was declared.
    ///
    /// `None` is unlimited, which is a legitimate choice for a queue whose work
    /// cannot poison and a visible one, because it is what leaving the clause
    /// out says. A record that reaches the ceiling stops being claimable and
    /// stays where it is: the dead letter is a predicate, not a second table.
    pub attempts: Option<u32>,
}

impl QueueDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields =
            BTreeMap::from([(FIELD_TIMEOUT.to_owned(), Value::Duration(self.timeout))]);
        // Written only when it was declared, on the bucket ceiling's contract
        // rather than a flag's: an attempt ceiling nobody named is absent rather
        // than zero, and zero is the one number that would have to mean
        // "unlimited" while reading as "never hand this out".
        if let Some(ceiling) = self.attempts {
            fields.insert(FIELD_ATTEMPTS.to_owned(), number(ceiling));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the timeout is missing or is not
    /// a duration, or when the attempt ceiling is not a number.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "queue";
        let fields = object(value, ENTITY)?;
        let Some(Value::Duration(timeout)) = fields.get(FIELD_TIMEOUT) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_TIMEOUT,
                found: fields
                    .get(FIELD_TIMEOUT)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self {
            timeout: *timeout,
            attempts: match fields.get(FIELD_ATTEMPTS) {
                Some(_) => Some(field_id(fields, FIELD_ATTEMPTS, ENTITY)?),
                None => None,
            },
        })
    }
}

/// The pair an edge table joins, and the order its edges are held in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeDeclaration {
    /// The table an edge may leave.
    pub from: TableId,
    /// The table an edge may arrive at.
    pub to: TableId,
    /// The order neighbours are held in, if one was declared.
    ///
    /// A **key-grammar** property rather than a query-time one: it becomes the
    /// suffix of the endpoint index's key, which is what makes "the ten most
    /// recent" a bounded read of adjacent keys instead of reading every edge and
    /// sorting. It is also why it cannot be changed later without rewriting
    /// every edge index in every store.
    pub order: Option<EdgeOrder>,
}

/// The order an edge table holds one node's edges in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeOrder {
    /// The edge field the order reads.
    pub field: String,
    /// Whether the order runs downward.
    pub descending: bool,
}

/// The parts of a stored table entry that together name its kind.
///
/// Grouped rather than passed as eight arguments, for the reason `TableShape`
/// already exists a few types above: four of them are `bool`, so the compiler
/// cannot tell one from another and a transposition produces a table of the
/// wrong kind with nothing anywhere in an error state. Reading these out of a
/// catalog record is the one place they all appear together.
#[derive(Debug, Clone, Default)]
pub struct StoredKind {
    /// The `edge` flag.
    pub edge: bool,
    /// The `bucket` flag.
    pub bucket: bool,
    /// The `collection` flag.
    pub collection: bool,
    /// The `geo` flag.
    pub geo: bool,
    /// An edge table's declared endpoints.
    pub endpoints: Option<EdgeDeclaration>,
    /// A vector store's declaration.
    pub vector: Option<VectorDeclaration>,
    /// A vault's wrapped key.
    pub vault: Option<VaultDeclaration>,
    /// A queue's timeout and attempt ceiling.
    pub queue: Option<QueueDeclaration>,
    /// A view's read.
    pub view: Option<ViewDeclaration>,
    /// A series table's retention.
    pub series: Option<SeriesDeclaration>,
    /// A bucket's size ceiling.
    pub ceiling: Option<u64>,
}

impl TableKind {
    /// The kind a stored definition's three flags describe.
    ///
    /// A catalog entry written before the kind existed carries the flags, and a
    /// build without the kind still writes them, so this is the only direction
    /// that needs a decision — and the decision is to **refuse** a combination
    /// rather than to prefer one of them. A stored table claiming to be both a
    /// bucket and an edge is not a table this build can serve correctly under
    /// either reading, and picking one would put a store into the state the
    /// kind exists to make unrepresentable. It is the same contract
    /// `identity_kind` already keeps for a word it does not recognise.
    ///
    /// An entry with no flags set is a plain table, which is what every entry
    /// written before any of these flags existed is.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when more than one kind is claimed.
    pub fn from_parts(stored: StoredKind) -> Result<Self> {
        let StoredKind {
            edge,
            bucket,
            collection,
            geo,
            endpoints,
            vector,
            vault,
            queue,
            view,
            series,
            ceiling,
        } = stored;
        // A vault is read first and alone. Every other arm below distinguishes
        // kinds that differ in what a caller may do; this one differs in
        // whether the records can be read at all, so a definition that both
        // carries a vault key and claims another kind is not a puzzle to
        // resolve by precedence — it is a catalog entry that must not be
        // honoured in either direction.
        if let Some(declared) = vault {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &queue, ceiling,
            ) {
                (false, false, false, false, None, None, None, None) => Ok(Self::Vault(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A series sets no flag either, and is pulled out here for the reason
        // the queue below it is: the arms already written stay the exhaustive
        // statement they are.
        if let Some(declared) = series {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &queue, &view, ceiling,
            ) {
                (false, false, false, false, None, None, None, None, None) => {
                    Ok(Self::Series(declared))
                }
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A queue sets no flag either, so it reaches the match below as a plain
        // table carrying a declaration — and it is pulled out here rather than
        // added as a ninth tuple element so that the arms already written keep
        // reading as the exhaustive statement they are.
        if let Some(declared) = queue {
            return match (
                edge, bucket, collection, geo, endpoints, vector, &view, ceiling,
            ) {
                (false, false, false, false, None, None, None, None) => Ok(Self::Queue(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        // A view sets no flag either, and is pulled out here for the reason the
        // queue is: the tuple match below is an exhaustive statement about the
        // kinds that *are* flags, and growing it by one element per declaration
        // would make every arm harder to read to say nothing new.
        if let Some(declared) = view {
            return match (edge, bucket, collection, geo, endpoints, vector, ceiling) {
                (false, false, false, false, None, None, None) => Ok(Self::View(declared)),
                _ => Err(Error::CatalogMalformed {
                    entity: "table",
                    field: "kind",
                    found: "more than one kind",
                }),
            };
        }
        match (edge, bucket, collection, geo, endpoints, vector, ceiling) {
            (false, false, false, false, None, None, None) => Ok(Self::Table),
            (true, false, false, false, endpoints, None, None) => Ok(Self::Edge(endpoints)),
            // The ceiling rides through with the flag, so a bucket declared
            // before the clause existed arrives with `None` and is the
            // unbounded bucket it has always been. A ceiling on any other kind
            // falls to the refusal below, because nothing else has a file to
            // measure it against.
            (false, true, false, false, None, None, ceiling) => Ok(Self::Bucket(ceiling)),
            (false, false, true, false, None, None, None) => Ok(Self::Collection),
            (false, false, false, true, None, None, None) => Ok(Self::Geo),
            // A vector store sets no flag, so it arrives here as a plain table
            // carrying a declaration. Any flag beside that declaration is two
            // kinds claimed at once and is refused with the rest.
            (false, false, false, false, None, Some(declared), None) => Ok(Self::Vector(declared)),
            _ => Err(Error::CatalogMalformed {
                entity: "table",
                field: "kind",
                found: "more than one kind",
            }),
        }
    }
}

impl VaultDeclaration {
    /// The value written inside the table's catalog entry.
    ///
    /// Two opaque byte strings. Nothing here is a secret — the wrapped key is
    /// ciphertext under the store's master key, and the identifier is a random
    /// name rather than anything derived from key material — so the catalog can
    /// hold them the way it holds any other declaration.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (
                FIELD_KEY_ID.to_owned(),
                Value::Bytes(self.key.key_id.bytes().to_vec()),
            ),
            (
                FIELD_WRAPPED.to_owned(),
                Value::Bytes(self.key.sealed.clone()),
            ),
        ]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when either part is missing or holds
    /// the wrong type. A vault whose key cannot be read is refused rather than
    /// treated as a vault with no key: the second reads as an empty store and
    /// would let a caller declare fields on it and write records that nothing
    /// could ever open.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "vault";
        let fields = object(value, ENTITY)?;
        let Some(Value::Bytes(key_id)) = fields.get(FIELD_KEY_ID) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "missing or not bytes",
            });
        };
        let key_id = <[u8; KeyId::BYTES]>::try_from(key_id.as_slice()).map_err(|_| {
            Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "the wrong number of bytes",
            }
        })?;
        let Some(Value::Bytes(sealed)) = fields.get(FIELD_WRAPPED) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_WRAPPED,
                found: "missing or not bytes",
            });
        };
        Ok(Self {
            key: Wrapped {
                key_id: KeyId::adopt(key_id),
                sealed: sealed.clone(),
            },
        })
    }
}

impl EdgeDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(self.from.get())),
            (FIELD_TO.to_owned(), number(self.to.get())),
        ]);
        if let Some(order) = &self.order {
            fields.insert(FIELD_ORDER.to_owned(), Value::from(order.field.as_str()));
            fields.insert(FIELD_DESCENDING.to_owned(), Value::Bool(order.descending));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// A direction without a field is refused rather than read as an unordered
    /// edge table: the two are different key grammars, and reading one as the other
    /// would answer a bounded neighbour read from an index that does not hold
    /// the order it claims.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when an endpoint is missing or holds
    /// the wrong type, or when the order is only half present.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "edge endpoints")?;
        let order = match fields.get(FIELD_ORDER) {
            None if fields.contains_key(FIELD_DESCENDING) => {
                return Err(Error::CatalogMalformed {
                    entity: "edge endpoints",
                    field: FIELD_ORDER,
                    found: "a direction with no field to order by",
                });
            }
            None => None,
            Some(Value::String(field)) => Some(EdgeOrder {
                field: field.clone(),
                descending: flag(fields, FIELD_DESCENDING, "edge endpoints")?,
            }),
            Some(other) => {
                return Err(Error::CatalogMalformed {
                    entity: "edge endpoints",
                    field: FIELD_ORDER,
                    found: other.type_name(),
                });
            }
        };
        Ok(Self {
            from: TableId::new(field_id(fields, FIELD_FROM, "edge endpoints")?),
            to: TableId::new(field_id(fields, FIELD_TO, "edge endpoints")?),
            order,
        })
    }
}

/// What a `DEFINE INDEX` says beyond which values it projects.
///
/// A struct rather than two booleans, for the reason `ids.rs` gives for
/// newtyping a `u32`: `create_index(id, "by_body", fields, false, true)`
/// compiles just as well transposed, and would make a unique index where a
/// search index was meant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexShape {
    /// Whether a value may appear more than once.
    pub unique: bool,
    /// Whether the index holds terms rather than whole values.
    pub search: bool,
    /// The distance a vector index is built with, when it is one.
    pub vector: Option<VectorDistance>,
    /// Whether the index holds cells of each record's geometry.
    pub spatial: bool,
}

/// Which distance a vector index's graph is built and searched with.
///
/// **The index declares it, and there is no default**, because a default would
/// silently decide which queries the index can serve. A graph whose edges were
/// chosen by one distance approximates that distance and no other: cosine
/// measures an angle and euclidean measures a separation, and for vectors nobody
/// normalised they rank differently. Serving a cosine query from a euclidean
/// graph would return plausible neighbours that are not the nearest — the exact
/// failure this whole node is arranged to prevent.
///
/// A read whose distance does not match the index's gets the scan, which is
/// exact, and says so through the access path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorDistance {
    /// The angle between two vectors, as `1 - cos θ`.
    Cosine,
    /// The distance between two points.
    Euclidean,
}

impl VectorDistance {
    /// How it is written in a definition, and stored in the catalog.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::Euclidean => "euclidean",
        }
    }

    /// The distance this word names.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "cosine" => Some(Self::Cosine),
            "euclidean" => Some(Self::Euclidean),
            _ => None,
        }
    }
}

/// An index on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinition {
    /// The index's id, which every entry's key carries.
    pub id: IndexId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// The table it is on.
    pub table: TableId,
    /// Its name, unique within that table.
    pub name: String,
    /// The values it indexes, in the order they are encoded.
    ///
    /// A path rather than a name, because an index may project a value nested
    /// inside the record: `address.city` is as indexable as `email`.
    ///
    /// Order is part of the index's identity: an index on `(a, b)` answers a
    /// query about `a` and one on `(b, a)` does not.
    pub fields: Vec<Path>,
    /// Whether this index holds **terms** rather than whole values.
    ///
    /// A search index projects one posting per term the analyzer finds, where an
    /// ordered index projects one entry per record. Which analyzer is used is
    /// the **field's** declaration, not this index's — see
    /// [`tessari_types::Analyzer`] for why that distinction is the whole design.
    pub search: bool,
    /// Whether a value may appear more than once.
    ///
    /// A unique index enforces it through the key layout — its entries carry no
    /// record id, so a second record with the same value writes the same key.
    pub unique: bool,
    /// The distance this index's graph is built with, when it is a vector index.
    ///
    /// It answers "which records are nearest this one" and nothing else, the way
    /// a search index answers a term and nothing else. It is the one index in
    /// this store whose answer is **approximate**, which is why a statement has
    /// to ask for it by name before it may serve one — and why the distance is
    /// declared rather than assumed.
    pub vector: Option<VectorDistance>,
    /// Whether this index holds the **cells** covering each record's geometry.
    ///
    /// One entry per cell rather than one per record, because a geometry is an
    /// extent and a cell is not: a shape wide enough to need several cells gets
    /// several entries, and the set of them is what a box query scans. The
    /// entry's value carries the record's own bounding box, so the filter step
    /// can reject a candidate without decoding the geometry.
    ///
    /// A cell match is therefore a **candidate and never a result** — the cells
    /// are coarser than the box and the box is coarser than the shape.
    pub spatial: bool,
}

impl IndexDefinition {
    /// Whether this index's entries are ordered by the indexed **value**.
    ///
    /// The question every reader wanting a lookup, a range or an order is
    /// actually asking, and it is phrased so the answer is **no by default**.
    ///
    /// That phrasing is the point. Each kind writes a different key: an ordered
    /// index writes the value, a search index writes terms, a vector index
    /// writes graph nodes, a spatial index writes cells. A reader that asks
    /// instead which kinds to *exclude* has to name every one of them, and every
    /// new kind is then a defect in every such site until each is found — the
    /// site keeps compiling, the plan still says `Index`, and the read returns
    /// **fewer rows with nothing raised**, because it looked up a value in a
    /// keyspace that is not keyed by values.
    ///
    /// That is not hypothetical. It shipped twice: a vector index made
    /// `WHERE embedding = [1, 2]` answer zero where the scan answered one, and a
    /// spatial index did the same for a geometry, each because one enumeration
    /// of kinds to skip was written before that kind existed. One predicate, and
    /// a kind that forgets to update it is excluded rather than admitted.
    #[must_use]
    pub const fn is_ordered(&self) -> bool {
        !self.search && !self.spatial && self.vector.is_none()
    }

    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(
                    self.fields
                        .iter()
                        .map(|field| Value::from(field.to_string().as_str()))
                        .collect(),
                ),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(self.unique)),
            (FIELD_SEARCH.to_owned(), Value::Bool(self.search)),
            (FIELD_SPATIAL.to_owned(), Value::Bool(self.spatial)),
            (
                FIELD_VECTOR.to_owned(),
                self.vector
                    .map_or(Value::None, |held| Value::from(held.name())),
            ),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "index")?;
        let malformed = |field: &'static str, found: &'static str| Error::CatalogMalformed {
            entity: "index",
            field,
            found,
        };
        let Some(Value::Array(names)) = fields.get(FIELD_FIELDS) else {
            return Err(malformed(
                FIELD_FIELDS,
                fields.get(FIELD_FIELDS).map_or("none", Value::type_name),
            ));
        };
        let indexed = names
            .iter()
            .map(|name| match name {
                // Stored as the text it was written as, so a definition made
                // before paths existed reads back as a one-step path and a dump
                // stays legible. Text that is not a path is corruption rather
                // than a bad request: nothing that reached the catalog could
                // have been one.
                Value::String(text) => {
                    Path::parse(text).ok_or_else(|| malformed(FIELD_FIELDS, "an unreadable path"))
                }
                other => Err(malformed(FIELD_FIELDS, other.type_name())),
            })
            .collect::<Result<Vec<Path>>>()?;
        let Some(Value::Bool(unique)) = fields.get(FIELD_UNIQUE) else {
            return Err(malformed(
                FIELD_UNIQUE,
                fields.get(FIELD_UNIQUE).map_or("none", Value::type_name),
            ));
        };
        Ok(Self {
            id: IndexId::new(field_id(fields, FIELD_ID, "index")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "index")?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, "index")?),
            table: TableId::new(field_id(fields, FIELD_TABLE, "index")?),
            name: field_name(fields, "index")?,
            fields: indexed,
            unique: *unique,
            // An index written before search existed is an ordered one, so
            // nothing on disk has to be migrated.
            search: flag(fields, FIELD_SEARCH, "index")?,
            // An index written before vector indexes existed holds no such
            // field and is not one, the same way one written before search was
            // an ordered index.
            vector: match fields.get(FIELD_VECTOR) {
                Some(Value::String(word)) => Some(VectorDistance::parse(word).ok_or_else(
                    || Error::CatalogMalformed {
                        entity: "index",
                        field: FIELD_VECTOR,
                        found: "a name that is not a distance",
                    },
                )?),
                _ => None,
            },
            // An index written before spatial indexes existed holds no such
            // field and is not one, the same reading the two flags above get.
            spatial: flag(fields, FIELD_SPATIAL, "index")?,
        })
    }
}

/// An identifier as it is stored: an integer, widened rather than cast.
pub(crate) fn number(id: u32) -> Value {
    Value::Number(Number::Integer(i64::from(id)))
}

/// A byte count as it is stored.
///
/// Saturating rather than fallible, and the saturation is unreachable: the only
/// way a ceiling enters the catalog is a `MAX n` literal, and this language's
/// whole numbers are `i64`, so a value past `i64::MAX` has no spelling. A
/// `Result` here would be a branch no input can take.
fn byte_count(value: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
}

/// The byte ceiling a table's entry carries, when it carries one.
///
/// Absent reads as no ceiling rather than as a fault, which is what every entry
/// written before the clause existed is. Present-but-not-a-positive-count is a
/// fault, because a stored zero would have to mean either "unbounded" or
/// "accepts nothing" and the entry does not say which.
fn ceiling(fields: &BTreeMap<String, Value>) -> Result<Option<u64>> {
    let malformed = || Error::CatalogMalformed {
        entity: "table",
        field: FIELD_CEILING,
        found: "not a whole number of bytes above zero",
    };
    match fields.get(FIELD_CEILING) {
        None => Ok(None),
        Some(Value::Number(Number::Integer(held))) => u64::try_from(*held)
            .ok()
            .filter(|held| *held > 0)
            .map(Some)
            .ok_or_else(malformed),
        Some(_) => Err(malformed()),
    }
}

/// A record counter as it is stored.
///
/// The ceiling belongs to the key grammar rather than to this function: a record
/// identity is a [`tessari_types::RecordId::Int`], an `i64`, so a count past
/// `i64::MAX` could be held here and could never be spent. It is refused where
/// it is produced, which is the only place the refusal can still say something
/// useful.
pub(crate) fn count(value: u64) -> Result<Value> {
    let held = i64::try_from(value).map_err(|_| Error::IdSpaceExhausted {
        level: RECORD_LEVEL,
    })?;
    Ok(Value::Number(Number::Integer(held)))
}

/// A record counter read back out of a stored value.
///
/// A negative integer is malformed rather than wrapped into an enormous count:
/// nothing writes one, so one being there means this record is not what this
/// build takes it for, and reading it as `18446744073709551615` would hand the
/// table an identity space it has already spent.
pub(crate) fn count_of(value: &Value, entity: &'static str, field: &'static str) -> Result<u64> {
    let malformed = |found: &'static str| Error::CatalogMalformed {
        entity,
        field,
        found,
    };
    let Value::Number(Number::Integer(raw)) = value else {
        return Err(malformed(value.type_name()));
    };
    u64::try_from(*raw).map_err(|_| malformed("number"))
}

/// The level a record counter reports when it runs out.
///
/// Public because the caller that turns a counter into a
/// [`tessari_types::RecordId`] narrows a `u64` to an `i64` to do it, and must
/// name the same level in the refusal. That narrowing cannot fail — [`count`]
/// refuses to store a number past `i64::MAX`, so a number this store answered
/// is a number it can spend — but "cannot fail" is not something to write an
/// `unwrap` on, and the refusal it would need already exists here.
pub const RECORD_LEVEL: &str = "record";

pub(crate) fn object<'a>(
    value: &'a Value,
    entity: &'static str,
) -> Result<&'a BTreeMap<String, Value>> {
    match value {
        Value::Object(fields) => Ok(fields),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_ID,
            found: other.type_name(),
        }),
    }
}

/// An identifier read back out of a stored value.
///
/// An integer too wide for the identifier is refused rather than narrowed: a
/// truncated id addresses a different entity, and nothing downstream could tell.
pub(crate) fn id_of(value: &Value, entity: &'static str, field: &'static str) -> Result<u32> {
    let malformed = |found: &'static str| Error::CatalogMalformed {
        entity,
        field,
        found,
    };
    let Value::Number(Number::Integer(raw)) = value else {
        return Err(malformed(value.type_name()));
    };
    u32::try_from(*raw).map_err(|_| malformed("number"))
}

pub(crate) fn field_id(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<u32> {
    match fields.get(field) {
        Some(value) => id_of(value, entity, field),
        None => Err(Error::CatalogMalformed {
            entity,
            field,
            found: "none",
        }),
    }
}

/// A boolean property of a definition.
///
/// Absent reads as `false`, so an entry written before the property existed is
/// readable rather than refused. A value of the wrong type is **not** read as
/// `false`: something wrote a well-formed value that is not a flag, which is an
/// integrity problem, and defaulting it would silently drop a constraint.
///
/// One reader for every flag, because a second copy that drifted would not fail
/// to compile — it would change what a table is.
pub(crate) fn flag(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<bool> {
    match fields.get(field) {
        Some(Value::Bool(declared)) => Ok(*declared),
        None => Ok(false),
        Some(other) => Err(Error::CatalogMalformed {
            entity,
            field,
            found: other.type_name(),
        }),
    }
}

/// How a table names a record the caller did not name.
///
/// Absent reads as [`IdentityKind::Int`] — the flags' contract, for the same
/// reason: a table written before the property existed is one that used the
/// default, not one whose declaration is unreadable.
///
/// A word this build does not recognise is **refused**, which is the one place
/// this differs from a flag. An unknown flag can only be a `true` nobody wrote;
/// an unknown identity scheme is a table already naming records some other way,
/// and reading it as `int` would put two schemes in one table.
fn identity_kind(fields: &BTreeMap<String, Value>, entity: &'static str) -> Result<IdentityKind> {
    match fields.get(FIELD_IDENTITY) {
        None => Ok(IdentityKind::Int),
        Some(Value::String(word)) => IdentityKind::parse(word).ok_or(Error::CatalogMalformed {
            entity,
            field: FIELD_IDENTITY,
            found: "an unknown identity scheme",
        }),
        Some(other) => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_IDENTITY,
            found: other.type_name(),
        }),
    }
}

pub(crate) fn field_name(fields: &BTreeMap<String, Value>, entity: &'static str) -> Result<String> {
    match fields.get(FIELD_NAME) {
        Some(Value::String(name)) => Ok(name.clone()),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_NAME,
            found: other.map_or("none", Value::type_name),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The bytes a namespace held before the replication clause existed.
    ///
    /// Pinned as a literal rather than produced by an encoder, so that this is
    /// genuinely a stored value from an older build and not a round trip of
    /// today's. A namespace written then must read as **never stated** — which
    /// is the true reading of a store that had no way to say anything, not a
    /// fallback — and must encode back to exactly the same object, so nothing
    /// already on disk is rewritten by being read.
    #[test]
    fn a_namespace_stored_before_the_clause_reads_as_never_stated() {
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
        ]));
        let read = NamespaceDefinition::from_value(&stored).unwrap();
        assert_eq!(read.replication, None);
        assert_eq!(read.to_value(), stored, "a read must not rewrite it");
    }

    #[test]
    fn a_stated_class_round_trips_and_a_definition_without_one_reads_as_silence() {
        // G027 S2.1. The second half is the one that matters on disk: a
        // namespace written before this field existed must still decode, and
        // must decode as *never stated* rather than as either answer — the same
        // property the replication clause bought, and the same reason nothing
        // stored is rewritten.
        for class in [
            ReplicationClass::SingleLeader,
            ReplicationClass::MultiMaster,
        ] {
            let namespace = NamespaceDefinition {
                id: NamespaceId::new(7),
                name: "prod".to_owned(),
                replication: None,
                class: Some(class),
            };
            let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
            assert_eq!(read, namespace, "{class}");
        }

        // Pinned as a literal for its neighbour's reason: this is a namespace a
        // build without the class field wrote, not a round trip of today's, and
        // it must read as never stated and encode back unchanged.
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
        ]));
        let read = NamespaceDefinition::from_value(&stored).unwrap();
        assert_eq!(read.class, None);
        assert_eq!(read.to_value(), stored, "a read must not rewrite it");
    }

    #[test]
    fn a_stated_policy_round_trips_and_is_not_silence() {
        for policy in [
            Replication::None,
            Replication::Factor(core::num::NonZeroU32::new(3).unwrap()),
        ] {
            let namespace = NamespaceDefinition {
                id: NamespaceId::new(7),
                name: "prod".to_owned(),
                replication: Some(policy),
                class: None,
            };
            let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
            assert_eq!(read, namespace, "{policy}");
            assert_ne!(read.replication, None, "{policy}");
        }
    }

    /// A policy a later build understands and this one does not refuses rather
    /// than reading as silence — the same refusal a table's unknown identity
    /// scheme takes, and for the same reason: serving a namespace as *nobody
    /// ever declared one* when somebody did is the wrong answer given
    /// confidently.
    #[test]
    fn a_policy_this_build_does_not_know_refuses() {
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
            ("replication".to_owned(), Value::from("every-rack")),
        ]));
        let error = NamespaceDefinition::from_value(&stored).unwrap_err();
        assert!(
            matches!(
                error,
                Error::CatalogMalformed {
                    field: "replication",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn every_definition_round_trips_through_its_value() {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
            replication: None,
            class: None,
        };
        assert_eq!(
            NamespaceDefinition::from_value(&namespace.to_value()).unwrap(),
            namespace
        );

        let database = DatabaseDefinition {
            id: DatabaseId::new(3),
            namespace: NamespaceId::new(7),
            name: "orders".to_owned(),
        };
        assert_eq!(
            DatabaseDefinition::from_value(&database.to_value()).unwrap(),
            database
        );

        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "line_items".to_owned(),
            schemafull: true,
            graph: Some(GraphId::new(9)),
            // Deliberately not the default here either, for the same reason the
            // identity below is not: a kind that round trips through three flags
            // is only proven by a kind that is not the one an absent flag gives.
            // The ceiling is present for that same reason once more — an absent
            // one round trips through a field that was never written, which
            // proves the default rather than the encoding.
            kind: TableKind::Bucket(Some(5 * 1024 * 1024)),
            // Deliberately not the default: a field that never travels round
            // trips perfectly as long as both ends agree on what it is when
            // absent, which is exactly the bug this assertion is for.
            identity: IdentityKind::Uuid,
            conflict: None,
            shards: None,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
    }

    #[test]
    fn a_table_entry_written_before_identities_were_declared_names_records_with_a_counter() {
        // Every table already in a store predates the field, and each one is
        // already naming records with a counter. Reading them as anything else
        // would rename what the *next* record in them is called.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
        ]);
        let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
        assert_eq!(read.identity, IdentityKind::Int);
    }

    #[test]
    fn a_naming_scheme_this_build_does_not_know_is_refused_rather_than_read_as_the_default() {
        // The asymmetry with the test above is the whole point. *Absent* means a
        // store written before the field existed, and its answer is knowable.
        // *Present and unrecognised* means a store written by a later build, and
        // the one thing that must not happen is this build deciding the table
        // uses the scheme it happens to prefer and minting ids under it.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_IDENTITY.to_owned(), Value::from("ulid")),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");

        // And a scheme that is not even a word is refused for the same reason
        // rather than falling through a `match` on the string.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_IDENTITY.to_owned(), Value::Bool(true)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_table_entry_written_before_schemas_existed_reads_as_schemaless() {
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
        ]);
        let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
        assert!(!read.schemafull);
    }

    #[test]
    fn a_schemafull_flag_that_is_not_a_boolean_is_refused_rather_than_read_as_false() {
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_SCHEMAFULL.to_owned(), Value::from("yes")),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_stored_table_claiming_two_kinds_is_refused_rather_than_read_as_one_of_them() {
        // In memory a table has one kind and a second one cannot be spelled. On
        // disk it is still three separate booleans, so the pair *is* writable —
        // by a build that predates the kind, or by corruption — and the decoder
        // is the only place left that can refuse it. Picking a winner here would
        // be the worse failure: the table would read as an edge on one replica
        // and a bucket on another, from bytes both agree on.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_EDGE.to_owned(), Value::Bool(true)),
            (FIELD_BUCKET.to_owned(), Value::Bool(true)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("kind"), "{text}");
    }

    #[test]
    fn an_edge_table_round_trips_its_endpoints_and_the_order_its_edges_are_held_in() {
        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "follows".to_owned(),
            schemafull: false,
            graph: None,
            shards: None,
            kind: TableKind::Edge(Some(EdgeDeclaration {
                from: TableId::new(4),
                to: TableId::new(5),
                // Deliberately descending, and deliberately not the same table at
                // both ends: an order that round trips as `false` and endpoints
                // that round trip transposed both survive an assertion made with
                // the defaults.
                order: Some(EdgeOrder {
                    field: "at".to_owned(),
                    descending: true,
                }),
            })),
            identity: IdentityKind::Int,
            conflict: None,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );

        // And a declared pair with no declared order is a different value, not a
        // missing one — it reads back unordered rather than as the default order.
        let unordered = TableDefinition {
            kind: TableKind::Edge(Some(EdgeDeclaration {
                from: TableId::new(4),
                to: TableId::new(5),
                order: None,
            })),
            ..table.clone()
        };
        assert_eq!(
            TableDefinition::from_value(&unordered.to_value()).unwrap(),
            unordered
        );

        // And the bare `EDGE`, which is what every edge table declared before the
        // clause existed is: no endpoints field is written, and the entry reads
        // back permissive rather than as a pair nobody declared.
        let permissive = TableDefinition {
            kind: TableKind::Edge(None),
            ..table
        };
        assert_eq!(
            TableDefinition::from_value(&permissive.to_value()).unwrap(),
            permissive
        );
    }

    #[test]
    fn a_vector_store_round_trips_its_width_and_its_distance() {
        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "embeddings".to_owned(),
            schemafull: false,
            graph: None,
            shards: None,
            // Deliberately the second distance rather than the first: a store
            // that round tripped as `cosine` whatever it was declared with
            // survives an assertion made with the default.
            kind: TableKind::Vector(VectorDeclaration {
                dimension: 768,
                distance: VectorDistance::Euclidean,
            }),
            identity: IdentityKind::Int,
            conflict: None,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
    }

    #[test]
    fn a_stored_vector_store_naming_a_distance_this_build_has_not_is_refused() {
        // Refused rather than read as `cosine`, on the reasoning `identity_kind`
        // records for an unknown word: the entry describes a store already
        // searched some other way, and answering its nearest-neighbour question
        // from a graph built for a different geometry returns plausible
        // neighbours that are not the nearest — with nothing in an error state.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("embeddings")),
            (
                FIELD_VECTOR.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIMENSION.to_owned(), number(768)),
                    (FIELD_DISTANCE.to_owned(), Value::from("manhattan")),
                ])),
            ),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_stored_vector_store_that_also_claims_a_flag_is_refused() {
        // The same refusal the three flags already get, extended to the kind
        // that is carried by a declaration instead of by a flag: on disk both
        // are writable side by side, and picking a winner would make one replica
        // read a bucket where another reads a vector store, from bytes they
        // agree on.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("embeddings")),
            (FIELD_BUCKET.to_owned(), Value::Bool(true)),
            (
                FIELD_VECTOR.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIMENSION.to_owned(), number(768)),
                    (FIELD_DISTANCE.to_owned(), Value::from("cosine")),
                ])),
            ),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("kind"), "{text}");
    }

    #[test]
    fn stored_endpoints_with_a_direction_but_no_field_are_refused_rather_than_read_as_unordered() {
        // The order is the endpoint index's key suffix, so reading a half-written
        // order as "no order" would answer a bounded neighbour read from an index
        // that does not hold the order it is being asked for — in the right
        // sequence often enough, by accident, to look correct.
        let endpoints = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(4)),
            (FIELD_TO.to_owned(), number(5)),
            (FIELD_DESCENDING.to_owned(), Value::Bool(true)),
        ]);
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("follows")),
            (FIELD_EDGE.to_owned(), Value::Bool(true)),
            (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("order"), "{text}");
    }

    #[test]
    fn a_stored_pair_on_a_table_that_is_not_an_edge_table_is_refused() {
        // The pair only means anything on an edge table, and this is the state
        // the kind exists to keep unrepresentable in memory — but disk is still
        // writable by an older build or by corruption. Reading it as a plain
        // table would silently discard a declared refusal, and reading it as an
        // edge table would give one replica a declared pair where another has
        // a table, from bytes both agree on.
        let endpoints = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(4)),
            (FIELD_TO.to_owned(), number(5)),
        ]);
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("follows")),
            (FIELD_EDGE.to_owned(), Value::Bool(false)),
            (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_missing_a_field_is_refused_and_names_it() {
        let fields = BTreeMap::from([(FIELD_ID.to_owned(), number(1))]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("namespace"), "{text}");
    }

    #[test]
    fn an_index_definition_round_trips_a_route_through_the_text_it_is_stored_as() {
        // The catalog stores a path as its spelling, so a path that read back as
        // a different route would index one value and filter another — and both
        // sides would look right in isolation.
        let index = IndexDefinition {
            id: IndexId::new(2),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            table: TableId::new(11),
            name: "by_home_city".to_owned(),
            fields: vec![
                Path::parse("address.city").expect("a path"),
                Path::parse("tags[0].name").expect("a path"),
                Path::field("email"),
            ],
            unique: false,
            search: false,
            spatial: false,
            vector: None,
        };
        assert_eq!(
            IndexDefinition::from_value(&index.to_value()).unwrap(),
            index
        );
    }

    #[test]
    fn an_index_entry_written_before_paths_existed_reads_as_a_single_field() {
        // Every definition on disk today spells one plain field name, and a plain
        // field name is a path of one step. Nothing has to be migrated.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(2)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_TABLE.to_owned(), number(11)),
            (FIELD_NAME.to_owned(), Value::from("by_email")),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(vec![Value::from("email")]),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(true)),
        ]);
        let read = IndexDefinition::from_value(&Value::Object(fields)).unwrap();
        assert_eq!(read.fields, vec![Path::field("email")]);
    }

    #[test]
    fn a_stored_route_that_is_not_a_route_is_corruption_rather_than_a_bad_request() {
        // Nothing that reached the catalog could have been an unreadable path:
        // the parser produced it, and the parser cannot spell one. So finding one
        // means the bytes changed underneath, which is a different failure from a
        // caller asking for something impossible.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(2)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_TABLE.to_owned(), number(11)),
            (FIELD_NAME.to_owned(), Value::from("by_broken")),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(vec![Value::from("address..city")]),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(false)),
        ]);
        let error = IndexDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_that_is_not_an_object_is_refused() {
        let error = NamespaceDefinition::from_value(&Value::from("prod")).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_record_count_round_trips_through_the_value_it_is_stored_as() {
        let stored = count(9_001).unwrap();
        assert_eq!(count_of(&stored, "record sequence", "next").unwrap(), 9_001);
    }

    #[test]
    fn a_record_count_the_key_grammar_could_never_spend_is_refused_where_it_is_produced() {
        // A record identity is an `i64`. A count past that could be held here
        // and could never become an identity, so the refusal belongs at the
        // write rather than at the read that would have to explain it.
        let error = count(u64::MAX).unwrap_err();
        assert!(matches!(error, Error::IdSpaceExhausted { .. }), "{error}");
    }

    #[test]
    fn a_stored_record_count_that_is_negative_is_refused_rather_than_wrapped() {
        // Nothing writes one, so one being present means the record is not what
        // this build takes it for — and wrapping would hand the table an
        // identity space it has already spent.
        let error = count_of(
            &Value::Number(Number::Integer(-1)),
            "record sequence",
            "next",
        )
        .unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn an_id_too_large_for_the_identifier_width_is_refused_rather_than_truncated() {
        let fields = BTreeMap::from([
            (
                FIELD_ID.to_owned(),
                Value::Number(Number::Integer(i64::from(u32::MAX) + 1)),
            ),
            (FIELD_NAME.to_owned(), Value::from("x")),
        ]);
        assert!(NamespaceDefinition::from_value(&Value::Object(fields)).is_err());
    }
}
