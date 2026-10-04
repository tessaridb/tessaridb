//! A table's definition and the shape it declares.

use super::ShardMap;
use super::{
    EdgeDeclaration, FIELD_AUTO_SPLIT, FIELD_BUCKET, FIELD_CEILING, FIELD_COLLECTION,
    FIELD_CONFLICT, FIELD_DATABASE, FIELD_EDGE, FIELD_ENDPOINTS, FIELD_EVENTS, FIELD_GEO,
    FIELD_GRAPH, FIELD_ID, FIELD_IDENTITY, FIELD_NAME, FIELD_NAMESPACE, FIELD_PARTITION,
    FIELD_QUEUE, FIELD_SCHEMAFULL, FIELD_SERIES, FIELD_SHARDS, FIELD_SPACE, FIELD_SPREAD,
    FIELD_TOPIC, FIELD_VAULT, FIELD_VECTOR, FIELD_VIEW, QueueDeclaration, SeriesDeclaration,
    StoredKind, TableKind, VaultCustody, VaultDeclaration, VectorDeclaration, ViewDeclaration,
    byte_count, ceiling, field_id, field_name, flag, identity_kind, number, object,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{
    ConflictPolicy, DatabaseId, GraphId, IdentityKind, NamespaceId, RecordId, TableId, Value,
};

mod encoding;

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
    /// Whether a generated identity begins with a bucket (`IDENTITY uuid
    /// SPREAD`, ADR-0113 D1); written only when set.
    pub spread: bool,
    /// When the store line's leader splits and merges the table's shards
    /// itself (`ALTER TABLE … SPLIT AUTOMATICALLY`, ADR-0113 D2); written only
    /// when set.
    pub auto_split: Option<super::AutoSplit>,
    /// What runs after each write of one of its records (ADR-0110), in name
    /// order; written only when there is one, so an entry without events is
    /// the bytes it always was.
    pub events: Vec<super::EventDeclaration>,
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
    /// Whether a generated identity begins with a bucket of two hex digits,
    /// so new records spread over the table's shards (`IDENTITY uuid SPREAD`,
    /// ADR-0113 D1).
    pub spread: bool,
}
