//! An index's definition and shape, and the distances a vector index measures.

use super::{
    EngineMember, FIELD_DATABASE, FIELD_ENGINE, FIELD_FIELDS, FIELD_ID, FIELD_NAME,
    FIELD_NAMESPACE, FIELD_OFFSETS, FIELD_POSITIONS, FIELD_SEARCH, FIELD_SPATIAL, FIELD_TABLE,
    FIELD_UNIQUE, FIELD_UNSCORED, FIELD_VECTOR, field_id, field_name, flag, number, object,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{DatabaseId, IndexId, NamespaceId, Path, TableId, Value};

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
    /// What a search index keeps beside its postings.
    pub costs: SearchCosts,
}

/// What a search index keeps beside its postings (ADR-0100 D4).
///
/// Each one changes what a read costs and never what it answers. The default is
/// what every search index held before these existed — counted postings and the
/// collection's statistics, and neither positions nor offsets — so an index
/// written before them reads back as exactly that and nothing on disk moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchCosts {
    /// `POSITIONS`: each posting carries the term's token ordinals.
    pub positions: bool,
    /// `OFFSETS`: each posting carries the term's byte ranges.
    pub offsets: bool,
    /// `NO SCORE`: membership postings, no collection statistics, and a score
    /// over the index refused as over no index at all.
    pub unscored: bool,
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
    /// What it keeps beside its postings, when it is a search index.
    pub costs: SearchCosts,
    /// The search this index is a member of, when it is one (ADR-0105).
    ///
    /// A member holds postings over **several** fields analysed with the
    /// search's own analyzer, so no field-index reader may take it for one of
    /// its own: `search` is false on a member and [`Self::is_ordered`] answers
    /// no for it.
    pub engine: Option<EngineMember>,
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
        !self.search && !self.spatial && self.vector.is_none() && self.engine.is_none()
    }

    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value = BTreeMap::from([
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
                FIELD_POSITIONS.to_owned(),
                Value::Bool(self.costs.positions),
            ),
            (FIELD_OFFSETS.to_owned(), Value::Bool(self.costs.offsets)),
            (FIELD_UNSCORED.to_owned(), Value::Bool(self.costs.unscored)),
            (
                FIELD_VECTOR.to_owned(),
                self.vector
                    .map_or(Value::None, |held| Value::from(held.name())),
            ),
        ]);
        // Written only on a member, so every other index's entry keeps the
        // bytes it always had.
        if let Some(engine) = &self.engine {
            value.insert(FIELD_ENGINE.to_owned(), engine.to_value());
        }
        Value::Object(value)
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
            // Written before the options existed: absent, so the default — a
            // scored index with neither positions nor offsets, which is what it
            // is.
            costs: SearchCosts {
                positions: flag(fields, FIELD_POSITIONS, "index")?,
                offsets: flag(fields, FIELD_OFFSETS, "index")?,
                unscored: flag(fields, FIELD_UNSCORED, "index")?,
            },
            engine: fields
                .get(FIELD_ENGINE)
                .map(EngineMember::from_value)
                .transpose()?,
        })
    }
}
