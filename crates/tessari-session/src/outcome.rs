//! What a statement answers with.
//!
//! Four shapes, because the language has four shapes of answer and collapsing
//! them into one would make every caller ask what it got back. A statement that
//! answers nothing says so rather than returning an empty list, which would be
//! indistinguishable from a read that found nothing.

use tessari_types::{RecordId, Value};

use crate::plan::Plan;

/// The result of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The statement did its work and has nothing to report.
    Done,
    /// Records, in key order, each with its identity, and how they were found.
    ///
    /// The path is reported rather than inferred because it is the difference
    /// between a read that stays fast as the table grows and one that does not.
    /// A caller that never sees it cannot tell them apart until it is slow.
    Records {
        /// What was found.
        records: Vec<(RecordId, Value)>,
        /// How — the same structure `EXPLAIN` answers with, for the read that
        /// actually ran.
        plan: Plan,
        /// What the store did that the records alone do not show.
        ///
        /// Empty for almost every read, which is the point: a note is worth
        /// reading because it is rare.
        notes: Vec<Note>,
        /// Whether the read said `ONLY`, and so answers with the record rather
        /// than a list holding it.
        ///
        /// A flag beside the records rather than an [`Self::Value`], because the
        /// two things `Value` would drop — the plan and the notes — are exactly
        /// what a read owes its caller. `records` holds at most one when this is
        /// set; the read refuses before it gets here otherwise.
        only: bool,
    },
    /// One value — or [`Value::None`] when the key holds nothing.
    ///
    /// `None` and a stored `Null` are different answers, which is the point of
    /// the value system keeping both.
    Value(Value),
    /// Keys, in order.
    Keys(Vec<RecordId>),
    /// How many records a conditional delete removed.
    ///
    /// A count rather than [`Outcome::Done`], because the whole point of a
    /// retention statement is how much it took: "removed 12 043 readings" is an
    /// operator checking their policy did what they meant, and `done` is that
    /// operator running a `SELECT count(*)` before and after to find out.
    Removed {
        /// How many.
        count: u64,
    },
}

/// How a read reached its records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPath {
    /// Straight to one record by its identity.
    Record,
    /// Through an index.
    Index,
    /// Through an index read in the order the statement asked for, stopping at
    /// its bound.
    ///
    /// Its own path rather than [`Self::Index`]: this one is chosen by the
    /// `ORDER BY` and the `LIMIT` rather than by a condition, and it is the only
    /// one that falls back — an index that cannot fill the bound leaves the
    /// answer needing records it does not hold, and reports the scan that then
    /// ran.
    Ordered,
    /// Every record of the table was read and tested.
    ///
    /// Correct, and linear in the size of the table. A text search reports this
    /// until a text index exists to serve it; the statement does not change when
    /// one does.
    Scan,
    /// Through a vector index, which answers with the best the graph found
    /// rather than provably the best there is.
    ///
    /// Its own path rather than [`Self::Index`], because it is the one read in
    /// this store an index answers *differently* from a scan. The answer also
    /// carries [`Note::Approximate`]; this is the same fact where a caller
    /// grouping by cost will look for it.
    Approximate,
    /// A walk from record to record along an edge table, one index read a step.
    ///
    /// Not [`Self::Index`], even though every step is one: which index runs is
    /// not a choice — an edge table is given one on each endpoint when it is
    /// declared — so reporting `index` invited the question of *which*, and
    /// there is no answer that is not the schema.
    Graph,
    /// Two reads brought together on a key.
    ///
    /// Each side reached its own records its own way, and neither of those is
    /// how this answer was reached. Reporting one side's path named half a read.
    Join,
    /// Records an inner read produced, held and then read from.
    ///
    /// The outer statement performed no access of its own, which is exactly what
    /// this says. The inner read's own path is a plan of its own and is not
    /// folded in here — a nested plan is its own feature and inventing one field
    /// for it would describe only the shallowest case.
    Materialised,
}

/// Something the store did on the way to an answer that the answer does not say.
///
/// A third channel, beside the records and the error. It exists because the two
/// it sits between cannot carry this: an error would refuse an answer that is
/// correct, and the records are correct, so silence is the only other option and
/// silence is what makes a fallback folklore. Every variant here is a case where
/// the store knows something the reader would otherwise have to guess at or
/// measure.
///
/// A note never changes the answer. A caller that ignores every note gets
/// exactly the records it would have got before notes existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The planner chose one path and the read took another.
    ///
    /// An ordered index that cannot fill the statement's bound leaves the answer
    /// needing records it does not hold, so the read scans instead. That is
    /// correct and it is linear, and the difference between the two is the whole
    /// reason anybody builds the index.
    FellBack {
        /// What the planner chose.
        from: AccessPath,
        /// What the read did instead.
        to: AccessPath,
    },
    /// The answer is the best the walk found, not provably the best there is.
    ///
    /// The one read in this store an index answers differently from a scan.
    /// Without this note the difference is invisible: an approximate answer and
    /// an exact one are the same shape, the same length, and usually the same
    /// records.
    Approximate,
    /// The read compared values of two different kinds.
    ///
    /// A schemaless store lets one record hold a number where the next holds the
    /// text of one, and `WHERE age = 30` then matches some of them. Nothing goes
    /// wrong — the comparison is well defined and the answer is right for the
    /// values that are there — and the read quietly answers a narrower question
    /// than the one that was asked.
    ///
    /// An absence never raises this. A record without the field is how a
    /// schemaless read narrows rather than fails, and a note on it would fire on
    /// nearly every read in the language.
    ComparedAcrossKinds {
        /// One kind, whichever sorts first, so the note reads the same way
        /// whichever side it was written on.
        left: &'static str,
        /// The other.
        right: &'static str,
    },
    /// A materialised source produced as many records as its ceiling allows.
    ///
    /// Its answer is therefore a prefix of what the inner read would have
    /// answered unbounded, and the outer statement asked its question of that
    /// prefix. `LIMIT` is the caller's own word, so this is not a mistake — but
    /// a bound that was reached and a bound that was not are different answers
    /// and look identical.
    SubqueryCeiling {
        /// The ceiling, which is also how many records it held.
        rows: u64,
    },
}

impl Note {
    /// A short stable name, for a client that groups or filters notes.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::FellBack { .. } => "fell-back",
            Self::Approximate => "approximate",
            Self::ComparedAcrossKinds { .. } => "compared-across-kinds",
            Self::SubqueryCeiling { .. } => "subquery-ceiling",
        }
    }

    /// The note in the words a reader would want it in.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::FellBack { from, to } => format!(
                "the {} path could not fill the bound, so the read took the {} path instead",
                from.name(),
                to.name(),
            ),
            Self::Approximate => {
                "an approximate index answered this, so a nearer record may exist".to_owned()
            }
            Self::ComparedAcrossKinds { left, right } => format!(
                "this read compared a {left} with a {right}, \
                 so it answered about the records whose kinds happened to line up",
            ),
            Self::SubqueryCeiling { rows } => format!(
                "the materialised source reached its ceiling of {rows}, \
                 so this answers about a prefix of what it would hold unbounded",
            ),
        }
    }
}

impl AccessPath {
    /// Every path, so the words can be listed and looked up.
    ///
    /// Rust cannot enumerate an enum's variants, so this is written out and a
    /// test pins its length against the count. A variant missing from here would
    /// not be *wrong* — it would be unassertable by `USING` and unlistable in
    /// the refusal that names the words, which is the quiet kind of gap.
    pub const ALL: [Self; 8] = [
        Self::Record,
        Self::Index,
        Self::Ordered,
        Self::Scan,
        Self::Approximate,
        Self::Graph,
        Self::Join,
        Self::Materialised,
    ];

    /// The path a word names, if it names one.
    ///
    /// Case-insensitive, because a statement writing `USING SCAN` in the case of
    /// its keywords is saying the same thing. Read through [`Self::name`] rather
    /// than through a second list of spellings: there is one vocabulary and this
    /// is the direction that reads it backwards.
    #[must_use]
    pub fn named(word: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|path| path.name().eq_ignore_ascii_case(word))
    }

    /// The words that exist, in the order [`Self::ALL`] lists them.
    #[must_use]
    pub fn known() -> String {
        Self::ALL
            .iter()
            .map(|path| path.name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A short stable name, for logs and for a client that shows the cost.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Index => "index",
            Self::Ordered => "ordered",
            Self::Scan => "scan",
            Self::Approximate => "approximate",
            Self::Graph => "graph",
            Self::Join => "join",
            Self::Materialised => "materialised",
        }
    }
}

impl Outcome {
    /// The records this outcome carries, if it carries any.
    #[must_use]
    pub fn records(&self) -> Option<&[(RecordId, Value)]> {
        match self {
            Self::Records { records, .. } => Some(records),
            _ => None,
        }
    }

    /// The plan the read took, if this outcome carries records.
    #[must_use]
    pub const fn plan(&self) -> Option<&Plan> {
        match self {
            Self::Records { plan, .. } => Some(plan),
            _ => None,
        }
    }

    /// How the records were found, if this outcome carries records.
    ///
    /// The access path alone, for a caller that wants the one word and not the
    /// structure around it.
    #[must_use]
    pub const fn path(&self) -> Option<AccessPath> {
        match self {
            Self::Records { plan, .. } => Some(plan.access),
            _ => None,
        }
    }

    /// What the store has to say about how it answered.
    ///
    /// Empty rather than `None` for an outcome that carries no notes at all, so
    /// a caller that renders them writes one loop and no branch.
    #[must_use]
    pub fn notes(&self) -> &[Note] {
        match self {
            Self::Records { notes, .. } => notes,
            _ => &[],
        }
    }

    /// Whether the read said `ONLY`, and so answers with the one record rather
    /// than a list holding it.
    ///
    /// `false` for every other outcome, which is what they are: a read that did
    /// not claim to answer with one.
    #[must_use]
    pub const fn only(&self) -> bool {
        match self {
            Self::Records { only, .. } => *only,
            _ => false,
        }
    }

    /// The single value this outcome carries, if it carries one.
    #[must_use]
    pub const fn value(&self) -> Option<&Value> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }

    /// The keys this outcome carries, if it carries any.
    #[must_use]
    pub fn keys(&self) -> Option<&[RecordId]> {
        match self {
            Self::Keys(keys) => Some(keys),
            _ => None,
        }
    }
}
