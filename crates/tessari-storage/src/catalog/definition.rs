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
    DatabaseId, GraphId, IdentityKind, IndexId, NamespaceId, Number, Path, TableId, Value,
};

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
const FIELD_ENDPOINTS: &str = "endpoints";
const FIELD_FROM: &str = "from";
const FIELD_TO: &str = "to";
const FIELD_ORDER: &str = "order";
const FIELD_DESCENDING: &str = "descending";
const FIELD_IDENTITY: &str = "identity";
const FIELD_GRAPH: &str = "graph";
const FIELD_DIMENSION: &str = "dimension";
const FIELD_DISTANCE: &str = "distance";

/// A namespace: the outermost tenancy level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceDefinition {
    /// The namespace's id.
    pub id: NamespaceId,
    /// Its name, which may change without moving anything.
    pub name: String,
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
        self.kind == TableKind::Bucket
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
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
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
        let fields = object(value, "namespace")?;
        Ok(Self {
            id: NamespaceId::new(field_id(fields, FIELD_ID, "namespace")?),
            name: field_name(fields, "namespace")?,
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
            // Still three named flags on disk. The kind is how this build talks
            // about a table, not a change to how one is stored, so no catalog
            // entry is touched, no migration step is owed, and a build without
            // the kind reads everything this one writes.
            (
                FIELD_EDGE.to_owned(),
                Value::Bool(matches!(self.kind, TableKind::Edge(_))),
            ),
            (
                FIELD_BUCKET.to_owned(),
                Value::Bool(self.kind == TableKind::Bucket),
            ),
            (
                FIELD_COLLECTION.to_owned(),
                Value::Bool(self.kind == TableKind::Collection),
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
            kind: TableKind::from_parts(
                flag(fields, FIELD_EDGE, "table")?,
                flag(fields, FIELD_BUCKET, "table")?,
                flag(fields, FIELD_COLLECTION, "table")?,
                match fields.get(FIELD_ENDPOINTS) {
                    Some(value) => Some(EdgeDeclaration::from_value(value)?),
                    None => None,
                },
                match fields.get(FIELD_VECTOR) {
                    Some(value) => Some(VectorDeclaration::from_value(value)?),
                    None => None,
                },
            )?,
            identity: identity_kind(fields, "table")?,
            graph: match fields.get(FIELD_GRAPH) {
                Some(_) => Some(GraphId::new(field_id(fields, FIELD_GRAPH, "table")?)),
                None => None,
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
    Bucket,
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
}

/// What a vector store calls the field its vectors are in.
///
/// Fixed rather than named in the declaration, because a store whose vector
/// field could be called anything is a store every reader has to look up before
/// writing to it — and the statement already says the whole of what the field
/// is. It is the same name as the store's index, which does not collide: fields
/// and indexes are separate namespaces.
pub const VECTOR_FIELD: &str = "vector";

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
    pub fn from_parts(
        edge: bool,
        bucket: bool,
        collection: bool,
        endpoints: Option<EdgeDeclaration>,
        vector: Option<VectorDeclaration>,
    ) -> Result<Self> {
        match (edge, bucket, collection, endpoints, vector) {
            (false, false, false, None, None) => Ok(Self::Table),
            (true, false, false, endpoints, None) => Ok(Self::Edge(endpoints)),
            (false, true, false, None, None) => Ok(Self::Bucket),
            (false, false, true, None, None) => Ok(Self::Collection),
            // A vector store sets no flag, so it arrives here as a plain table
            // carrying a declaration. Any flag beside that declaration is two
            // kinds claimed at once and is refused with the rest.
            (false, false, false, None, Some(declared)) => Ok(Self::Vector(declared)),
            _ => Err(Error::CatalogMalformed {
                entity: "table",
                field: "kind",
                found: "more than one kind",
            }),
        }
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

    #[test]
    fn every_definition_round_trips_through_its_value() {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
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
            kind: TableKind::Bucket,
            // Deliberately not the default: a field that never travels round
            // trips perfectly as long as both ends agree on what it is when
            // absent, which is exactly the bug this assertion is for.
            identity: IdentityKind::Uuid,
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
            // Deliberately the second distance rather than the first: a store
            // that round tripped as `cosine` whatever it was declared with
            // survives an assertion made with the default.
            kind: TableKind::Vector(VectorDeclaration {
                dimension: 768,
                distance: VectorDistance::Euclidean,
            }),
            identity: IdentityKind::Int,
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
