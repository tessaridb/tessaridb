//! The functions the language has, and how many arguments each one takes.
//!
//! # What earns a place
//!
//! **A function is here when it cannot be expressed by what the language already
//! has.** That is why there is no `array::contains` (`CONTAINS` says it), no
//! `string::contains` (`LIKE '%x%'` says it), and no `is_none` (`= NONE` says
//! it). The rule keeps the surface from growing by association and gives a
//! reviewer one question to ask about any addition.
//!
//! `array::last` is the clearest case for the rule rather than against it: a
//! path takes a literal position, and there is no length to subtract from, so
//! "the last element" is genuinely unsayable without it.
//!
//! # Why a name is namespaced
//!
//! `string::len` rather than `len`, for three reasons: the function surface can
//! never collide with a field name, the set is groupable in the documentation,
//! and an aggregate called `count` later does not have to argue with
//! `array::len`. It costs one punctuation token.
//!
//! # Why arity lives here and types do not
//!
//! The set of functions is known when a statement is read, so a call with the
//! wrong number of arguments is a mistake that can be refused before anything
//! runs. What each argument *holds* is not known until a record is in hand, so
//! that check belongs where the value is.

use core::fmt;

/// Whether a function answers the same way every time it is asked.
///
/// # What this is for, concretely
///
/// A reader in `tessari-session` decides whether an expression may be evaluated
/// **once above the records** instead of once per record, and today it decides
/// by asking whether the expression reads a record. For every function the
/// language currently has, those two questions have the same answer, so nothing
/// has needed this.
///
/// They come apart at the first function that reads no record and must still be
/// asked again for every one of them — a generated identity being the obvious
/// one. Folded like a constant, `rand::uuid()` would hand every record of a bulk
/// insert **the same id**: not a compile error, not a test failure, and not
/// visible until two records that should differ do not. Reading no record is
/// therefore not the same property as being foldable, and this is the one that
/// actually governs it.
///
/// The reverse mistake is the same shape. `time::now()` reads no record and
/// *should* fold: unfolded, one statement observes several instants and
/// `ORDER BY time::now()` sorts by a key regenerated under its own comparator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Purity {
    /// The same arguments answer the same way, always.
    ///
    /// Which is what lets the answer be computed once, reordered, or served from
    /// an index without changing what the statement means.
    Pure,
    /// Impure, and fixed for the length of one statement.
    ///
    /// A statement observes **one** instant, so every `time::now()` in it
    /// answers alike however many records it is asked about. Foldable, and
    /// folding is what currently delivers that — see `plan::fold`.
    PerStatement,
}

/// One of the language's own functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Function {
    /// `string::len(text)` — how many characters, not bytes.
    StringLen,
    /// `string::lower(text)`
    StringLower,
    /// `string::upper(text)`
    StringUpper,
    /// `string::trim(text)` — whitespace from both ends.
    StringTrim,
    /// `string::concat(a, b)`
    StringConcat,
    /// `array::len(items)`
    ArrayLen,
    /// `array::first(items)` — `none` when there are none.
    ArrayFirst,
    /// `array::last(items)` — the one a path cannot reach.
    ArrayLast,
    /// `math::abs(number)`
    MathAbs,
    /// `math::floor(number)`
    MathFloor,
    /// `math::ceil(number)`
    MathCeil,
    /// `math::round(number)` — half away from zero.
    MathRound,
    /// `time::now()` — the instant the statement is evaluated at.
    TimeNow,
    /// `type::of(value)` — the type's name, as §3 spells it.
    TypeOf,
    /// `vector::cosine(a, b)` — the angle between two vectors, as a distance.
    VectorCosine,
    /// `vector::euclidean(a, b)` — the distance between two points.
    VectorEuclidean,
    /// `vector::dot(a, b)` — the inner product.
    VectorDot,
    /// `search::score(field, 'query')` — how well this record answers the query,
    /// measured against the collection the field's search index summarises.
    SearchScore,
    /// `time::bucket(instant, 1h)` — the start of the window that instant is in.
    TimeBucket,
    /// `geo::intersects(a, b)` — whether the two shapes share any position,
    /// boundaries included.
    GeoIntersects,
    /// `geo::disjoint(a, b)` — whether they share none.
    GeoDisjoint,
    /// `geo::covers(a, b)` — whether the whole of `b` lies in `a`, its boundary
    /// counting as part of it.
    GeoCovers,
    /// `geo::covered_by(a, b)` — [`Function::GeoCovers`] the other way round.
    GeoCoveredBy,
    /// `geo::contains(a, b)` — whether `a` holds the whole of `b` and meets more
    /// than its edge. The strict half of the pair: a position on a polygon's
    /// boundary is *covered by* it and not *contained in* it.
    GeoContains,
    /// `geo::within(a, b)` — [`Function::GeoContains`] the other way round.
    GeoWithin,
    /// `geo::equals(a, b)` — whether the two cover exactly the same positions,
    /// however each was written.
    GeoEquals,
    /// `geo::touches(a, b)` — whether they meet and their interiors do not. Two
    /// positions never touch, and a position touches a path only at an end.
    GeoTouches,
    /// `geo::distance(a, b)` — how far apart two positions are along the
    /// ellipsoid, in **metres**. Both arguments must be positions; there is no
    /// distance between larger shapes yet.
    GeoDistance,
    /// `geo::area(shape)` — how much ground a shape covers, in **square
    /// metres**. Zero for anything with no interior.
    GeoArea,
}

impl Function {
    /// Every function, so that a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::StringLen,
        Self::StringLower,
        Self::StringUpper,
        Self::StringTrim,
        Self::StringConcat,
        Self::ArrayLen,
        Self::ArrayFirst,
        Self::ArrayLast,
        Self::MathAbs,
        Self::MathFloor,
        Self::MathCeil,
        Self::MathRound,
        Self::TimeNow,
        Self::TypeOf,
        Self::VectorCosine,
        Self::VectorEuclidean,
        Self::VectorDot,
        Self::SearchScore,
        Self::TimeBucket,
        Self::GeoIntersects,
        Self::GeoDisjoint,
        Self::GeoCovers,
        Self::GeoCoveredBy,
        Self::GeoContains,
        Self::GeoWithin,
        Self::GeoEquals,
        Self::GeoTouches,
        Self::GeoDistance,
        Self::GeoArea,
    ];

    /// How the function is written, group and name together.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::StringLen => "string::len",
            Self::StringLower => "string::lower",
            Self::StringUpper => "string::upper",
            Self::StringTrim => "string::trim",
            Self::StringConcat => "string::concat",
            Self::ArrayLen => "array::len",
            Self::ArrayFirst => "array::first",
            Self::ArrayLast => "array::last",
            Self::MathAbs => "math::abs",
            Self::MathFloor => "math::floor",
            Self::MathCeil => "math::ceil",
            Self::MathRound => "math::round",
            Self::TimeNow => "time::now",
            Self::TypeOf => "type::of",
            Self::VectorCosine => "vector::cosine",
            Self::VectorEuclidean => "vector::euclidean",
            Self::VectorDot => "vector::dot",
            Self::SearchScore => "search::score",
            Self::TimeBucket => "time::bucket",
            Self::GeoIntersects => "geo::intersects",
            Self::GeoDisjoint => "geo::disjoint",
            Self::GeoCovers => "geo::covers",
            Self::GeoCoveredBy => "geo::covered_by",
            Self::GeoContains => "geo::contains",
            Self::GeoWithin => "geo::within",
            Self::GeoEquals => "geo::equals",
            Self::GeoTouches => "geo::touches",
            Self::GeoDistance => "geo::distance",
            Self::GeoArea => "geo::area",
        }
    }

    /// How many arguments it takes.
    #[must_use]
    /// # Every function is named, and there is no catch-all
    ///
    /// There used to be a `_ => 1`, and it is the arm that nearly shipped a
    /// one-argument `geo::touches`: the variant was added, the compiler named
    /// only the matches that were already exhaustive, and this one answered
    /// with a number that was simply wrong. The parser would then have refused
    /// every correctly written call and accepted a malformed one, and the only
    /// thing that would have caught it is that somebody wrote the conformance
    /// case.
    ///
    /// So a function added to the language will not compile until somebody says
    /// how many arguments it takes.
    pub const fn arity(self) -> usize {
        match self {
            Self::TimeNow => 0,
            Self::StringLen
            | Self::StringLower
            | Self::StringUpper
            | Self::StringTrim
            | Self::ArrayLen
            | Self::ArrayFirst
            | Self::ArrayLast
            | Self::MathAbs
            | Self::MathFloor
            | Self::MathCeil
            | Self::MathRound
            | Self::TypeOf
            | Self::GeoArea => 1,
            Self::StringConcat
            | Self::VectorCosine
            | Self::VectorEuclidean
            | Self::VectorDot
            | Self::SearchScore
            | Self::TimeBucket
            | Self::GeoIntersects
            | Self::GeoDisjoint
            | Self::GeoCovers
            | Self::GeoCoveredBy
            | Self::GeoContains
            | Self::GeoWithin
            | Self::GeoEquals
            | Self::GeoTouches
            | Self::GeoDistance => 2,
        }
    }

    /// Whether this function answers the same way every time it is asked.
    ///
    /// # Every function is named here too, and for [`Function::arity`]'s reason
    ///
    /// A catch-all arm would hand a newly added function the majority answer,
    /// and the majority answer is [`Purity::Pure`]. A function wrongly called
    /// pure is refused nowhere and fails no test — it quietly answers from a
    /// value something else decided to keep. So a function added to the language
    /// will not compile until somebody says which of these it is.
    ///
    /// [`Purity::PerStatement`] is a category of one today, and every function
    /// here is currently foldable. The test below writes that membership down so
    /// that the first function which is *not* — one that must be asked again for
    /// every record despite reading none — cannot be added without somebody
    /// reading what folding would do to it.
    #[must_use]
    pub const fn purity(self) -> Purity {
        match self {
            // The clock moves while a statement runs; the statement should not
            // see it move.
            Self::TimeNow => Purity::PerStatement,
            Self::StringLen
            | Self::StringLower
            | Self::StringUpper
            | Self::StringTrim
            | Self::StringConcat
            | Self::ArrayLen
            | Self::ArrayFirst
            | Self::ArrayLast
            | Self::MathAbs
            | Self::MathFloor
            | Self::MathCeil
            | Self::MathRound
            | Self::TypeOf
            | Self::VectorCosine
            | Self::VectorEuclidean
            | Self::VectorDot
            | Self::SearchScore
            | Self::TimeBucket
            | Self::GeoIntersects
            | Self::GeoDisjoint
            | Self::GeoCovers
            | Self::GeoCoveredBy
            | Self::GeoContains
            | Self::GeoWithin
            | Self::GeoEquals
            | Self::GeoTouches
            | Self::GeoDistance
            | Self::GeoArea => Purity::Pure,
        }
    }

    /// Whether this function has an answer for an argument that holds nothing.
    ///
    /// Most do not: a function of an absence is an absence, which is what lets a
    /// read over documents of differing shapes narrow instead of failing. Two
    /// kinds do:
    ///
    /// - [`Function::TypeOf`] asks *about* a value rather than computing from
    ///   one, and the type of an absence is `none`.
    /// - The distances answer `+∞`, because the distance to something that is
    ///   not there is unbounded — and because `NONE` sorts below every value, so
    ///   propagating it would make a bounded nearest-neighbour read answer with
    ///   the records that have no vector at all, in first place.
    /// - [`Function::SearchScore`] answers `0`: a record with no text in the
    ///   field holds none of the query's words, and a document holding none of
    ///   them scores zero. That is the computed answer and not a stand-in for
    ///   one.
    ///
    /// [`Function::GeoDistance`] is in the list for the distance reason and not
    /// by analogy: `ORDER BY geo::distance(…) LIMIT 10` over a table where some
    /// records have no shape would otherwise answer with exactly those records,
    /// in first place, because `NONE` sorts below every value. `geo::area` is
    /// **not** in the list — an area is not an ordering a bounded read is built
    /// on in the same way, and an absent shape having no area is a claim this
    /// store cannot make.
    #[must_use]
    pub const fn answers_for_absence(self) -> bool {
        matches!(
            self,
            Self::TypeOf
                | Self::VectorCosine
                | Self::VectorEuclidean
                | Self::VectorDot
                | Self::SearchScore
                | Self::GeoDistance
        )
    }

    /// The function a `group::name` spells, if there is one.
    #[must_use]
    pub fn parse(spelling: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|function| function.spelling() == spelling)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling())
    }
}

#[cfg(test)]
mod tests {
    use super::{Function, Purity};

    /// The whole membership of [`Purity::PerStatement`], asserted as a set.
    ///
    /// The guard the type system cannot give. `purity` forces a new function to
    /// be *classified*, but a classification on its own changes nothing about
    /// how the function is evaluated — `plan::fold` still decides by asking
    /// whether the expression reads a record, and a function that reads none is
    /// folded whatever it is classified as.
    ///
    /// So the set is written down here. Adding a member fails this test, and the
    /// failure is the instruction: go and read what folding does to it before
    /// widening this list.
    #[test]
    fn the_statement_constant_functions_are_exactly_the_one_the_fold_was_written_for() {
        let constant: Vec<&str> = Function::ALL
            .iter()
            .filter(|function| function.purity() == Purity::PerStatement)
            .map(|function| function.spelling())
            .collect();
        assert_eq!(constant, ["time::now"]);
    }

    #[test]
    fn every_function_is_classified_and_only_the_clock_is_not_pure() {
        for function in Function::ALL {
            let expected = if *function == Function::TimeNow {
                Purity::PerStatement
            } else {
                Purity::Pure
            };
            assert_eq!(function.purity(), expected, "{function} is misclassified");
        }
    }

    #[test]
    fn every_function_is_findable_by_its_own_spelling_and_no_two_share_one() {
        let mut spellings: Vec<&str> = Function::ALL
            .iter()
            .map(|function| {
                assert_eq!(Function::parse(function.spelling()), Some(*function));
                function.spelling()
            })
            .collect();
        let count = spellings.len();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), count, "two functions share a spelling");
    }

    #[test]
    fn a_name_that_is_not_a_function_is_not_one() {
        // Case-sensitive, unlike a keyword: a function name is a name, and every
        // other name in this language is case-sensitive too.
        assert_eq!(Function::parse("string::length"), None);
        assert_eq!(Function::parse("STRING::LEN"), None);
        assert_eq!(Function::parse("len"), None);
    }
}
