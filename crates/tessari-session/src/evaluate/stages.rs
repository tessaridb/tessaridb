//! What a read hands from one stage to the next.

use tessari_ql::Expr;
use tessari_storage::IndexDefinition;
use tessari_types::{RecordId, RecordRef, TableId, Value};

use crate::context::Context;
use crate::outcome::{Note, Suggestion};
use crate::plan::Plan;
use crate::search::Searched;

/// What a join produces, which is still a collection.
///
/// A join builds a map of one side and probes it with the other, so its work is
/// not per-record and streaming it would move the materialisation rather than
/// remove it. Named separately so the difference is visible in the signature
/// rather than resting on a comment.
pub(crate) type Joined = (Vec<(RecordId, Value)>, Plan, Searched);

/// What one hop over adjacency reached, and where the next hop starts.
///
/// Both halves are lists of the same length only by coincidence, and the second
/// is empty whenever the step named no node — so they are named rather than left
/// as a tuple two `Vec`s wide that a caller could read in either order.
pub(crate) type Hopped = (Vec<(RecordId, Value)>, Vec<RecordRef>);

/// The records a read reached, how it reached them, and whether reaching them
/// settled the condition.
///
/// A struct rather than a triple for the reason `Answered` is one: the last
/// field is a bare `bool` that a caller could silently drop or, worse, read the
/// wrong way round. Naming it makes `answered: false` — which is what a scan and
/// every ordinary index read say — a statement rather than a position.
/// What the statement itself said about the plan, as opposed to what the store
/// worked out.
///
/// The two travel together because they come from one place — the read's tail —
/// and are read by one function. A pair rather than two arguments because a
/// planner call taking eight things has stopped being readable, and grouping
/// them by where they came from is the division that survives the next one being
/// added.
#[derive(Clone, Copy)]
pub(crate) struct Asked<'a> {
    /// The table the plan reports, when the source names one.
    pub(crate) named: Option<&'a str>,
    /// `WITHOUT SCAN GUARD` — the planner's size veto is lifted for this read.
    pub(crate) lift_scan_guard: bool,
}

impl Asked<'_> {
    /// A read whose statement said nothing about its plan.
    ///
    /// A `DELETE` carries no read tail to say anything in, so it asks for the
    /// defaults rather than for a privilege no caller could have written down.
    pub(crate) const fn nothing() -> Self {
        Self {
            named: None,
            lift_scan_guard: false,
        }
    }
}

pub(crate) struct Reached {
    /// The records to test, or to answer with when `answered`.
    pub(crate) records: Candidates,
    /// How they were reached, as `EXPLAIN` would report it.
    pub(crate) plan: Plan,
    /// Whether the read has already settled the whole condition.
    ///
    /// `false` unless a search index answered a plain conjunction that was the
    /// entire `WHERE`, over a field this session may read — see
    /// [`Session::trusts`]. A caller that ignores this is correct and slower,
    /// which is the right way round for a field of this kind.
    pub(crate) answered: bool,
}

/// How an index-served read's candidates are available to the caller.
///
/// Every index read produces a **candidate set** the condition then refines, and
/// for most of them that set is built before the first record can be tested.
/// A range is the exception: its entries can be named in one pass and its
/// records read afterwards, so a caller that fills its bound can stop the fetch
/// it has not reached yet.
///
/// Why only the fetch, and not the entry walk: the answer is in record order —
/// a bounded read answers the records a scan of the same predicate answers, and
/// nothing else, which the tests in `bounded_index_reads.rs` pin — and the
/// lowest identity among the candidates is not known until every candidate has
/// been named. A walk that stopped early would answer with whichever records the
/// index reached first, which for an index whose order is not identity order is
/// a different set of records. So the entry walk runs to the end by
/// construction, and what the bound reaches is the half whose cost grows with
/// the answer.
pub(crate) enum Candidates {
    /// Built whole before the first one can be tested.
    Held(Vec<(RecordId, Value)>),
    /// A range the caller can walk, stopping where its answer fills.
    Range {
        index: Box<IndexDefinition>,
        fixed: Vec<Value>,
        lower: Option<Value>,
        upper: Option<Value>,
    },
}

/// What a vector walk came back with, and the index that answered it.
///
/// No `Walked` here: every empty return is a shape this walk does not serve — no
/// index on the path, one built for another distance, a query that is not a
/// vector — and none of them is an index that ran out.
pub(crate) type Approximated = (Vec<(RecordId, Value)>, String);

/// What a read produced, and what it has to say about how.
///
/// A struct rather than the tuple this was, because the third element is the one
/// a caller is most likely to drop on the floor — and a `_` in a tuple pattern
/// says nothing about what was dropped, while a named field does.
pub(crate) struct Answered {
    /// The records, in the order the statement asked for.
    pub records: Vec<(RecordId, Value)>,
    /// How they were reached — the plan the read took, in the structure
    /// `EXPLAIN` answers with.
    pub plan: Plan,
    /// What the read did that the records do not show.
    pub notes: Vec<Note>,
    /// What the query might have meant, when it named a term nothing holds.
    ///
    /// Carried from the searched context rather than computed here, because it
    /// is a fact about the query and the collection and not about the read: it
    /// is resolved before an access path exists, so that planning a read
    /// differently cannot give it a different suggestion.
    pub suggestion: Option<Suggestion>,
}

/// What an index-served walk came back with.
///
/// Three cases rather than an [`Option`], because coming back empty happens for
/// two unrelated reasons and only one of them is worth telling anybody about.
/// **No index holds this order** is the ordinary state of a table nobody has
/// indexed; **an index holds it and could not fill the bound** is the case the
/// index was built to prevent. Collapsed into `None` they are the same value,
/// and a note raised on it would fire on every unindexed read — which is how a
/// diagnostic becomes noise and then becomes ignored.
///
/// The planner cannot tell them apart either: `plan::ordered` reads the
/// statement and never the schema, so it says `Some` for an `ORDER BY … LIMIT`
/// over a table with no index at all.
pub(crate) enum Walked {
    /// The index answered, and named itself so the plan can report which one.
    Served {
        /// What it came back with.
        found: Vec<(RecordId, Value)>,
        /// The index that served it.
        index: String,
    },
    /// An index holds this order and could not fill the bound.
    Declined,
    /// No index holds this order, so nothing was given up.
    NotServed,
}

/// What resolving a source reached, and what producing it still needs.
///
/// Three cases rather than five, because what matters here is not which clause
/// was written but whether the records exist yet.
pub(crate) enum Prepared<'a> {
    /// A table, resolved to its tenancy. Nothing has been read.
    Table(Context, TableId),
    /// A table and the condition its records must satisfy. The condition is
    /// carried rather than re-matched out of the statement, so producing needs
    /// no arm that cannot happen.
    Filtered(Context, TableId, &'a Expr),
    /// A source whose records exist already, because reaching its context meant
    /// reading them: one record by identity, a traversal, a join. Each is a
    /// barrier in its own right — a join builds a map of one side — so producing
    /// lazily would move the materialisation rather than remove it.
    Held(Vec<(RecordId, Value)>, Plan),
}
