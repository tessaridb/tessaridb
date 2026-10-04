use super::*;

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
