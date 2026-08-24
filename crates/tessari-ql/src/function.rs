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
        }
    }

    /// How many arguments it takes.
    #[must_use]
    pub const fn arity(self) -> usize {
        match self {
            Self::TimeNow => 0,
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
            | Self::GeoEquals => 2,
            _ => 1,
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
    #[must_use]
    pub const fn answers_for_absence(self) -> bool {
        matches!(
            self,
            Self::TypeOf
                | Self::VectorCosine
                | Self::VectorEuclidean
                | Self::VectorDot
                | Self::SearchScore
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
    use super::Function;

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
