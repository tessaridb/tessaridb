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
        /// What the query might have meant, when a term nothing holds says it
        /// probably meant something else.
        ///
        /// `None` when no term dictionary was consulted — see [`Suggestion`],
        /// where the three states and the reason for them are set out.
        suggestion: Option<Suggestion>,
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
    /// A walk between two positions in the table's own keyspace.
    ///
    /// Not [`Self::Scan`], which reads every record, and not [`Self::Index`],
    /// which reads a second structure to find out which records to fetch. This
    /// one is the records themselves, in the span the statement named — the
    /// table's own key order **is** the ordering being used, so nothing is
    /// consulted and nothing outside the span is read.
    ///
    /// Its own word because the cost is its own: a scan is linear in the table
    /// and this is linear in the answer, which is the difference a caller
    /// reading a plan most wants to see.
    Span,
    /// Records an inner read produced, held and then read from.
    ///
    /// The outer statement performed no access of its own, which is exactly what
    /// this says. The inner read's own path is a plan of its own and is not
    /// folded in here — a nested plan is its own feature and inventing one field
    /// for it would describe only the shallowest case.
    Materialised,
}

/// Why a walk over a proximity graph is not provably the best answer there is.
///
/// One string, reached by both channels that state the same fact — the note a
/// caller may read and the exactness a caller cannot help reading. Two copies of
/// a sentence like this drift, and the drift is invisible: each channel is
/// individually correct and they disagree about the same read.
const GRAPH_WALK_IS_APPROXIMATE: &str =
    "an approximate index answered this, so a nearer record may exist";

/// Whether an answer is provably the records the question names.
///
/// # Why this is a field and not a note
///
/// [`Note::Approximate`] already says this, and says it well. What it cannot do
/// is make a caller *unable to miss it*, because a note is opt-in by
/// construction: a caller that reads none of them gets exactly the records it
/// would have got before notes existed. So an approximate answer and an exact
/// one are the same shape, the same length, usually the same records — and, to
/// a caller that never looks, the same claim.
///
/// The second failure is the one that outlives any single read. If exactness is
/// something a path *adds* when it happens to be approximate, then a path added
/// later that forgets reads as exact, because absence-means-exact is a default
/// nobody chose and nobody can see. That is why this is derived from the access
/// path by an exhaustive match in [`AccessPath::exactness`] rather than set at a
/// construction site: a new path does not compile until somebody says where it
/// sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exactness {
    /// Provably the records the question names.
    ///
    /// Not "no approximation was detected" — the read had no step that could
    /// answer with fewer or other records than the question asks for.
    Exact,
    /// Not provably, and why.
    ///
    /// The reason is carried rather than looked up from the path, because the
    /// value of a returned `false` is entirely in what follows it: a caller told
    /// only that an answer is inexact has learned that it cannot trust the
    /// answer and nothing about what to do instead.
    Approximate(&'static str),
}

impl Exactness {
    /// Whether the answer is provably the one the question names.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }

    /// Why it is not, when it is not.
    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Exact => None,
            Self::Approximate(why) => Some(why),
        }
    }
}

/// A term the query named that nothing holds, and the nearest term that is held.
///
/// Both spellings are **analyzed** terms rather than the words as typed, because
/// the near one has to be: it came out of the term dictionary, which holds what
/// the analyzer produced. Reporting the typed word beside a stored stem would
/// invite a caller to compare two things that were never the same kind, so the
/// typed side is the analyzed form of what the caller wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nearest {
    /// The term as the query asked for it, analyzed.
    pub typed: String,
    /// The nearest term the dictionary holds.
    pub instead: String,
}

/// What the store would have looked for, had the query asked for something the
/// collection holds.
///
/// # Why this is a field and not a note, and not the records
///
/// Not the records, because a suggestion is advice about a **different question**
/// than the one that was asked. Substituting it would answer a query nobody
/// wrote, and the store already knows why that is worse than useless here: a
/// term the records do not hold does not tie a BM25 ranking, it inverts it, so
/// the shortest document arrives first wearing a plausible score. The executed
/// query is byte-for-byte what the caller wrote, always.
///
/// Not a note, because [`Note`] crosses the wire as prose. A caller reading
/// "did you mean vector" has to parse English to recover the term, and a caller
/// that cannot cheaply read a suggestion as data is a caller that will glue it
/// into the next query by hand — the exact substitution this exists to prevent.
///
/// # The absence is three states, not two
///
/// This type is carried as an `Option`, and the `None` is load-bearing. A
/// suggestion needs a term dictionary, and only a `SEARCH` index has one, so a
/// query over an unindexed field cannot be asked this question at all. If
/// absence meant "nothing is near", that read would report a confident negative
/// it never checked. So `None` is *no dictionary was consulted*,
/// [`Self::NothingNearer`] is *one was, and every term is held*, and
/// [`Self::DidYouMean`] is *these were not*. It is the same shape, and the same
/// reason, as [`Exactness`] being written even when the answer is exact.
///
/// That a suggestion appears only where an index does is not a breach of the
/// rule that an index changes what a read costs and never what it answers. The
/// records are identical either way. A suggestion is not an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suggestion {
    /// The dictionary was consulted and holds every term the query named.
    ///
    /// Reported rather than left absent for the reason above: a caller has to be
    /// able to tell this from a read that never had a dictionary to ask.
    NothingNearer,
    /// Terms the dictionary does not hold, each with the nearest one it does.
    ///
    /// Never empty — a query with nothing to suggest reports
    /// [`Self::NothingNearer`] instead, so an empty list cannot come to mean two
    /// things.
    DidYouMean(Vec<Nearest>),
}

impl Suggestion {
    /// The corrections, when there are any.
    #[must_use]
    pub fn corrections(&self) -> &[Nearest] {
        match self {
            Self::NothingNearer => &[],
            Self::DidYouMean(nearest) => nearest,
        }
    }
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
    /// A cursor was applied to the records rather than sought to.
    ///
    /// `AFTER` exists to make a deep page cost what a shallow one costs, and it
    /// does that by starting the read past the anchor's own key — but only a
    /// read answering in the store's own key order has a key to start past. Any
    /// other read has to reach the records first and then keep the ones after
    /// the anchor, which is the work an offset does, spelled better.
    ///
    /// The answer is the same either way. The cost is not, and without this note
    /// the difference is invisible: a page that sought and a page that walked are
    /// the same records in the same order.
    ///
    /// What a walked page gives is the cursor's **correctness** — a page that
    /// does not shift when a record is inserted behind it — and not its cost.
    /// Measured on this store, a sought page is flat at about 13 µs from the
    /// first record to the hundred-thousandth while the offset it replaces grows
    /// from 10 µs to 39 ms; a walked page is the cost of the read it sits on,
    /// which is what the same statement paying an offset would have cost too.
    CursorWalked,
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
    /// A held read is most of the way to the ceiling that will refuse it.
    ///
    /// A view naming no `LIMIT` runs under the ceiling every held read runs
    /// under, and past it the read is refused rather than shortened. That is the
    /// right failure and it arrives with no warning: a view sitting just below
    /// the line reads perfectly today and stops working on an ordinary week's
    /// growth, with nothing having said so.
    ///
    /// Unlike every other note here, this one reports a **state** rather than
    /// something that happened during the read — so it fires on every read while
    /// the condition holds. That is deliberate: the condition is persistent, and
    /// a warning that appeared once and then went quiet would be worse than
    /// none.
    NearingCeiling {
        /// How many records the read held.
        rows: u64,
        /// The ceiling it is approaching, past which the read is refused.
        most: u64,
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
            Self::CursorWalked => "cursor-walked",
            Self::SubqueryCeiling { .. } => "subquery-ceiling",
            Self::NearingCeiling { .. } => "nearing-ceiling",
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
            Self::Approximate => GRAPH_WALK_IS_APPROXIMATE.to_owned(),
            Self::ComparedAcrossKinds { left, right } => format!(
                "this read compared a {left} with a {right}, \
                 so it answered about the records whose kinds happened to line up",
            ),
            Self::CursorWalked => "this page was reached by reading the records rather \
                 than seeking to the anchor, so it cost what the read costs and not \
                 what the page costs"
                .to_owned(),
            Self::SubqueryCeiling { rows } => format!(
                "the materialised source reached its ceiling of {rows}, \
                 so this answers about a prefix of what it would hold unbounded",
            ),
            Self::NearingCeiling { rows, most } => format!(
                "this held read holds {rows} records of the {most} it may hold, \
                 past which it is refused rather than shortened",
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
    pub const ALL: [Self; 9] = [
        Self::Record,
        Self::Index,
        Self::Ordered,
        Self::Scan,
        Self::Approximate,
        Self::Graph,
        Self::Join,
        Self::Span,
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

    /// Whether records reached this way are provably the ones the question
    /// names.
    ///
    /// **Exhaustive on purpose.** A wildcard arm here would make every path
    /// added afterwards exact by default, silently, which is exactly the claim
    /// nobody would have made on its behalf. Written out, a new variant is a
    /// compile error until somebody decides — and deciding is one line, while
    /// discovering the wrong default is a caller trusting an answer it should
    /// not have.
    ///
    /// Only one path is approximate today, and it is not the fuzzy one. A capped
    /// term expansion is **not offered as a candidate** and the scan answers, so
    /// `MATCHES PREFIX` and `MATCHES FUZZY` reach provably the records they name
    /// however wide the expansion would have been.
    #[must_use]
    pub const fn exactness(self) -> Exactness {
        match self {
            Self::Approximate => Exactness::Approximate(GRAPH_WALK_IS_APPROXIMATE),
            Self::Record
            | Self::Index
            | Self::Ordered
            | Self::Scan
            | Self::Graph
            | Self::Join
            | Self::Span
            | Self::Materialised => Exactness::Exact,
        }
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
            Self::Span => "span",
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

    /// Whether the records are provably the ones the question names.
    ///
    /// Beside [`Self::path`] for the same reason that one exists — a caller that
    /// wants the property and not the structure around it — and answering
    /// `None` only for an outcome that carries no records at all, which is an
    /// outcome that made no claim of this kind to begin with.
    #[must_use]
    pub const fn exactness(&self) -> Option<Exactness> {
        match self {
            Self::Records { plan, .. } => Some(plan.exact),
            _ => None,
        }
    }

    /// What the query might have meant, when it named a term nothing holds.
    ///
    /// Two nestings of absence, and they say different things. The outer `None`
    /// is an outcome carrying no records, which asked nothing of a dictionary
    /// because it ran no query. The inner one is a read that ran but had no
    /// dictionary to ask. Flattening them would let a `Removed` count and a
    /// scan over an unindexed field answer this question the same way.
    #[must_use]
    pub const fn suggestion(&self) -> Option<&Option<Suggestion>> {
        match self {
            Self::Records { suggestion, .. } => Some(suggestion),
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
