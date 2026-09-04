use core::cmp::Ordering;

use tessari_geo::{Bounds, Cell, Relation};
use tessari_storage::IndexDefinition;
use tessari_types::Value;

use crate::outcome::AccessPath;
use crate::plan::Plan;

/// What shape of test an index is being asked to answer.
///
/// Ordered by how much a candidate of this shape is trusted to narrow when
/// nothing exact is known: a single value beats a range, because a range can be
/// the whole table (`LIKE 'a%'`) and a value cannot be more than the records
/// holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Shape {
    /// `<path> = <constant>` — one entry, or one value's worth of them.
    Equality,
    /// `<path> MATCHES '<text>'` — the postings of every term, intersected.
    Terms,
    /// `<path> MATCHES PREFIX '<text>'` — for each word, the postings of every
    /// term it reaches, unioned; then those intersected across the words.
    ///
    /// Below [`Shape::Terms`] and above a value prefix, which is where the work
    /// puts it. It reads more posting lists than an exact term match — one per
    /// expanded term rather than one per word — but it reads them from the same
    /// structure with the same ceiling, where a value prefix walks an ordered
    /// index whose size is not knowable without doing the read.
    PrefixTerms,
    /// `<path> MATCHES FUZZY '<text>'` — for each word, the postings of every
    /// term within the edit budget that shares its mandatory prefix, unioned;
    /// then those intersected across the words.
    ///
    /// Beside [`Shape::PrefixTerms`] rather than below it, because the two
    /// promise the same thing by the same method: a union of posting lists per
    /// word, intersected across words, with a ceiling computed from real
    /// document frequencies. `Rows::AtMost` does the discriminating between
    /// them. Ranking fuzzy lower because its walk *reads* more terms would be a
    /// claim about how many rows it returns, which is a different quantity and
    /// one this store keeps no statistics to support.
    FuzzyTerms,
    /// `<path> MATCHES '<a> OR <b> <c>'` — for each `OR`-joined group, the
    /// postings of its terms, unioned; then those intersected across the groups.
    ///
    /// Beside the two above, and for the third time the same reason: the read is
    /// a union per group intersected across groups, with a ceiling computed from
    /// real document frequencies. What differs is only where the alternatives
    /// came from — a dictionary walk there, the query itself here — and that
    /// difference is in the name so `EXPLAIN` does not report a walk that never
    /// ran.
    AnyTerms,
    /// `<path> LIKE '<literal>%'` — a range over the values beginning with it.
    Prefix,
    /// `<path> < <constant>`, and the other three orderings — a bounded scan.
    ///
    /// Ranked beside a prefix and for the same reason: both can be the whole
    /// table, and neither's size is knowable without doing the read.
    Range,
    /// `geo::intersects(<path>, <constant>)`, and the five other relations a box
    /// test is a superset of — the cells a query box covers.
    ///
    /// Last, and conservatively so. A query box can be the world, so like a
    /// prefix and a range its size is not knowable without the read; unlike
    /// them, what it produces is a set of **candidates** to refine rather than a
    /// set of matches, so among shapes that promise equally little it is the one
    /// doing the most work per row it returns. Ranking it above anything would
    /// be a claim about selectivity this store keeps no statistics to make.
    Region,
}

/// What a chosen candidate hands the executor, ready to run.
///
/// A sum rather than a struct of optional fields, so that a prefix candidate
/// cannot exist without its prefix and a term candidate cannot exist without its
/// terms. The alternative — one `bound: Value` plus a `terms: Vec<String>` that
/// is empty for two shapes out of three — puts branches in the executor that
/// cannot be reached and therefore cannot be tested, which is how an unreachable
/// branch quietly becomes reachable.
///
/// It also means the work of deciding is not repeated: the literal prefix and
/// the analysed terms are computed once, while ranking, and travel to the read.
#[derive(Debug, Clone)]
pub(crate) enum Served {
    /// The values the index is looked up by — one per field it fixes, in field
    /// order, and always a **leading** run of them.
    ///
    /// A tuple rather than a value, because an index that could answer about
    /// two of its columns and was only ever asked about one is the cost §8 named
    /// twice. When the run reaches the index's arity the lookup is complete, and
    /// `Transaction::records_by_index` turns a complete lookup on a unique index
    /// into a point read — which is what makes a `UNIQUE` composite able to
    /// promise a ceiling of one at all.
    ///
    /// The run stops at the first field the condition does not fix with an
    /// equality, so `a = 1 AND b LIKE 'x%'` on `(a, b)` fixes `[1]`: the lookup
    /// takes exact values per field and a mixed tuple cannot be said in it. The
    /// condition re-tests the rest, as it does for every candidate anyway.
    Equality(Vec<Value>),
    /// The literal prefix the range starts at.
    Prefix(String),
    /// The terms whose postings are intersected.
    Terms(Vec<String>),
    /// The **expansions** whose postings are unioned and then intersected: one
    /// set of terms per word the query typed.
    ///
    /// Two levels deep because the question is two levels deep — every typed
    /// word must be matched by *some* term beginning with it. Flattening the
    /// sets into one list would lose which word each term came from and turn a
    /// conjunction of disjunctions into a single disjunction, which answers with
    /// every record holding any of the words.
    ///
    /// Resolved by the planner rather than at execution, because whether the
    /// index can serve the read at all depends on how far the expansions reach:
    /// past the cap, this candidate is not offered and the scan answers.
    PrefixTerms(Vec<Vec<String>>),
    /// The terms each fuzzy word reached — the same shape as
    /// [`Served::PrefixTerms`], and deliberately a separate variant.
    ///
    /// The executor treats the two identically, because by the time a walk has
    /// produced concrete terms there is nothing left to distinguish. `EXPLAIN`
    /// does not, and must not: telling a reader `prefix-terms` when a fuzzy walk
    /// ran would misreport which question the index answered.
    FuzzyTerms(Vec<Vec<String>>),
    /// The terms of each `OR`-joined group — the same shape again, and again a
    /// separate variant so `EXPLAIN` reports which question the index answered.
    ///
    /// A query's **excluded** terms are deliberately not here. Removing them
    /// would need the complement of a posting list, which the index cannot
    /// enumerate; what it can do is produce the records the positive groups
    /// reach, which is a superset of the answer, and let the condition drop the
    /// rest — the refinement every candidate on this path already gets.
    AnyTerms(Vec<Vec<String>>),
    /// The two ends of an ordered scan, either of which may be absent.
    ///
    /// Both are carried as **values**, not as byte bounds, because the index
    /// encoding normalises — `1` and `1.0` become the same bytes — so an
    /// exclusive end cannot be said in bytes at all. It does not need to be: the
    /// scan takes both ends inclusive and the condition that asked discards what
    /// it does not want, which it was going to do to every candidate anyway.
    Range {
        /// The leading run of the index's fields the condition fixes to exact
        /// values, with the ranged field immediately after it.
        ///
        /// Empty for a range on the index's own leading field, which is every
        /// range this store served before composite ranges existed. Non-empty
        /// for `a = 1 AND b > 2` on `(a, b)`, where the bounds apply *inside* the
        /// entries holding `a = 1` rather than across the whole index.
        fixed: Vec<Value>,
        /// The lower end, when the condition gave one.
        lower: Option<Value>,
        /// The upper end, when it gave one.
        upper: Option<Value>,
    },
    /// The cells a query box covers, the box itself, and which box test the
    /// predicate's semantics allow.
    ///
    /// All three travel together because all three are computed once, here,
    /// while ranking. The cells decide **where** the read looks; the box decides
    /// which of what it finds is worth fetching; the relation is what makes the
    /// box test a superset of the predicate rather than a second, subtly
    /// different predicate.
    Region {
        /// The covering of the query box.
        cells: Vec<Cell>,
        /// The query box, which the stored boxes are compared against.
        bounds: Bounds,
        /// Which comparison the predicate's semantics permit.
        relation: Relation,
    },
}

impl Served {
    /// Which shape this is.
    pub(crate) const fn shape(&self) -> Shape {
        match self {
            Self::Equality(_) => Shape::Equality,
            Self::Prefix(_) => Shape::Prefix,
            Self::Terms(_) => Shape::Terms,
            Self::PrefixTerms(_) => Shape::PrefixTerms,
            Self::FuzzyTerms(_) => Shape::FuzzyTerms,
            Self::AnyTerms(_) => Shape::AnyTerms,
            Self::Range { .. } => Shape::Range,
            Self::Region { .. } => Shape::Region,
        }
    }

    /// How many of the index's fields this lookup narrows.
    ///
    /// One for a shape that reaches only the leading field, the length of the
    /// run for an equality tuple, and for a range the fixed run **plus one** for
    /// the field it bounds. It is a **proof of narrowing**, not an estimate: the
    /// entries matching two fixed fields are a subset of those matching the
    /// first alone, and the entries a range keeps are a subset of the run it
    /// walks — whatever the data holds. That is why it ranks candidates and a
    /// guess would not.
    pub(crate) fn fixed(&self) -> usize {
        match self {
            Self::Equality(values) => values.len(),
            Self::Prefix(_)
            | Self::Terms(_)
            | Self::PrefixTerms(_)
            | Self::FuzzyTerms(_)
            | Self::AnyTerms(_)
            | Self::Region { .. } => 1,
            Self::Range { fixed, .. } => fixed.len().saturating_add(1),
        }
    }
}

/// How many records a candidate can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rows {
    /// A ceiling that was free to learn.
    AtMost(u64),
    /// No ceiling without doing the work the candidate would do anyway.
    Unknown,
}

impl Rows {
    /// Which of two bounds promises fewer records.
    ///
    /// Every known ceiling beats every unknown one, which is the whole ranking
    /// in a sentence. Two unknowns are equal here and the shape breaks the tie.
    pub(super) fn rank(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::AtMost(held), Self::AtMost(theirs)) => held.cmp(&theirs),
            (Self::AtMost(_), Self::Unknown) => Ordering::Less,
            (Self::Unknown, Self::AtMost(_)) => Ordering::Greater,
            (Self::Unknown, Self::Unknown) => Ordering::Equal,
        }
    }
}

/// One conjunct an index could serve, with everything the executor needs.
///
/// The bound is already evaluated, because ranking needs it — and evaluating it
/// again in the executor would let a `time::now()` in a filter mean two
/// different instants inside one statement.
///
/// It carries no path. Enumeration used the path to find the index and to reach
/// the field's analyzer; by the time a candidate exists both are on it, and a
/// field nothing reads is a field that goes out of step with what does.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    /// What the read needs, in the form that read takes.
    pub(crate) served: Served,
    /// The index that would serve it.
    pub(crate) index: IndexDefinition,
    /// How many records it can produce.
    pub(crate) rows: Rows,
}

impl Candidate {
    /// The plan this choice describes.
    ///
    /// One function, called by the read that runs the choice and by the
    /// `EXPLAIN` that only describes it. Two of them would report the same
    /// choice in different words the first time one changed, which is the whole
    /// failure this replaces.
    pub(crate) fn plan(&self, table: Option<&str>) -> Plan {
        Plan {
            table: table.map(ToOwned::to_owned),
            index: Some(self.index.name.clone()),
            shape: Some(self.served.shape().name()),
            columns: Some(u64::try_from(self.served.fixed()).unwrap_or(u64::MAX)),
            cells: match &self.served {
                Served::Region { cells, .. } => {
                    Some(u64::try_from(cells.len()).unwrap_or(u64::MAX))
                }
                _ => None,
            },
            at_most: match self.rows {
                Rows::AtMost(held) => Some(held),
                Rows::Unknown => None,
            },
            ..Plan::new(AccessPath::Index)
        }
    }
}

/// The word a shape answers under, for a plan somebody is reading.
impl Shape {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Equality => "equality",
            Self::Prefix => "prefix",
            Self::Range => "range",
            Self::Region => "region",
            Self::Terms => "terms",
            Self::PrefixTerms => "prefix-terms",
            Self::FuzzyTerms => "fuzzy-terms",
            Self::AnyTerms => "any-terms",
        }
    }
}
