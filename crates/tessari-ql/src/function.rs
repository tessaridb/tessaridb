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
//! The `type::` casts pass the rule for the same reason: text arriving from
//! outside is text, and nothing else in the language turns `'42'` into a number
//! or `'2026-01-01T00:00:00Z'` into an instant. Two candidates were **rejected**
//! while admitting them:
//!
//! - **`type::number`** names three numeric kinds without choosing one, so it
//!   could only mean "whichever kind the argument already had", which is not a
//!   conversion. [`FieldKind::Number`](tessari_types::FieldKind::Number) exists
//!   because a *declaration* can usefully accept all three; a cast cannot
//!   usefully produce all three.
//! - **`type::decimal`** is deferred rather than refused. An exact decimal is
//!   the kind money is kept in, so a cast that produced one from a float would
//!   have to say what it does with a value no decimal holds exactly — and that
//!   question deserves its own answer rather than an arm in this wave.
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
/// `plan::fold` decides whether an expression may be evaluated **once above the
/// records** instead of once per record, and it used to decide by asking only
/// whether the expression reads a record. For the twenty-nine functions the
/// language had before `rand::uuid`, those two questions had the same answer.
///
/// They come apart at the first function that reads no record and must still be
/// asked again for every one of them — a generated identity being the obvious
/// one. Folded like a constant, `rand::uuid()` hands every record of a bulk
/// insert **the same id**: not a compile error, not a test failure, and not
/// visible until two records that should differ do not. Reading no record is
/// therefore not the same property as being foldable, and this is the one that
/// actually governs it — which is why the fold now asks both questions.
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
    /// Impure, and a fresh answer every single time it is asked.
    ///
    /// The one answer that is **not** foldable, and the reason this enum is
    /// consulted rather than [`Purity::Pure`] being assumed. A generated
    /// identity is the case: `rand::uuid()` reads no record, so the fold's older
    /// question said constant, and every row of one read took the same id.
    PerCall,
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
    /// `string::split(text, separator)` — the parts between occurrences.
    StringSplit,
    /// `string::slice(text, start, count)` — a run of characters.
    StringSlice,
    /// `string::lines(text, start, count)` — a run of lines.
    StringLines,
    /// `string::replace(text, from, to)` — every occurrence replaced.
    StringReplace,
    /// `math::sqrt(number)` — the square root, as a float.
    MathSqrt,
    /// `math::pow(base, exponent)`
    MathPow,
    /// `array::len(items)`
    ArrayLen,
    /// `array::first(items)` — `none` when there are none.
    ArrayFirst,
    /// `array::last(items)` — the one a path cannot reach.
    ArrayLast,
    /// `object::keys(o)` — the field names, in the object's own order.
    ObjectKeys,
    /// `object::values(o)` — the values, in the same order as the names.
    ObjectValues,
    /// `object::len(o)` — how many fields.
    ObjectLen,
    /// `array::distinct(items)` — each value once, first occurrence kept.
    ArrayDistinct,
    /// `array::sort(items)` — ascending, in the value system's declared order.
    ArraySort,
    /// `array::reverse(items)` — the same values, back to front.
    ArrayReverse,
    /// `array::flatten(items)` — one level of nesting removed.
    ArrayFlatten,
    /// `array::join(items, separator)` — the elements as one text.
    ArrayJoin,
    /// `array::slice(items, start, count)` — a run of elements.
    ArraySlice,
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
    /// `time::year(instant)` — the calendar year, in UTC.
    TimeYear,
    /// `time::month(instant)` — the month, 1 through 12.
    TimeMonth,
    /// `time::day(instant)` — the day of the month, 1 through 31.
    TimeDay,
    /// `time::hour(instant)` — the hour, 0 through 23.
    TimeHour,
    /// `time::minute(instant)` — the minute, 0 through 59.
    TimeMinute,
    /// `time::second(instant)` — the second **of the minute**, 0 through 59.
    /// Not the seconds since the epoch, which is [`Function::TimeUnix`].
    TimeSecond,
    /// `time::unix(instant)` — whole seconds since the epoch.
    TimeUnix,
    /// `time::from_unix(seconds)` — the instant a second count names.
    TimeFromUnix,
    /// `rand::uuid()` — a fresh version-4 identifier, once per call.
    RandUuid,
    /// `type::of(value)` — the type's name, as §3 spells it.
    TypeOf,
    /// `type::bool(value)` — the value as a boolean, or a refusal.
    TypeBool,
    /// `type::int(value)` — the value as an integer, or a refusal.
    TypeInt,
    /// `type::float(value)` — the value as a float, or a refusal.
    TypeFloat,
    /// `type::string(value)` — the value as the text it reads back from.
    TypeString,
    /// `type::datetime(value)` — the value as an instant, or a refusal.
    TypeDatetime,
    /// `type::uuid(value)` — the value as a UUID, or a refusal.
    TypeUuid,
    /// `vector::cosine(a, b)` — the angle between two vectors, as a distance.
    VectorCosine,
    /// `vector::euclidean(a, b)` — the distance between two points.
    VectorEuclidean,
    /// `vector::dot(a, b)` — the inner product.
    VectorDot,
    /// `search::score(field, 'query')` — how well this record answers the query,
    /// measured against the collection the field's search index summarises.
    SearchScore,
    /// `search::highlight(field)` — where in this record's text the read's own
    /// query matched, as `{ start, end }` byte ranges.
    ///
    /// It takes the **field alone**. The query comes from what the statement
    /// asked of that field, so a highlight cannot disagree with the filter that
    /// selected the record — see the session's `search::highlight` for why a
    /// second copy of the query is the failure this signature avoids.
    SearchHighlight,
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
    /// `crypto::sha256(text)` — the SHA-256 digest of the text's UTF-8 bytes,
    /// as sixty-four lowercase hexadecimal characters.
    ///
    /// Text in and text out. A digest is compared, stored beside a record and
    /// printed in a log, and all three want the form every other tool prints;
    /// an array of thirty-two numbers would make `crypto::sha256(x) = '…'` —
    /// the sentence the function exists for — unwritable.
    ///
    /// **Not a password hash.** These are fast by design, which is the property
    /// a credential must not be stored under. Passwords go through
    /// `crate::identity`, whose parameters are pinned, and there is deliberately
    /// no callable-from-a-query path to it (Q-218).
    CryptoSha256,
    /// `crypto::sha512(text)` — the same, as a hundred and twenty-eight
    /// lowercase hexadecimal characters.
    CryptoSha512,
}

impl Function {
    /// Every function, so that a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::StringLen,
        Self::StringLower,
        Self::StringUpper,
        Self::StringTrim,
        Self::StringConcat,
        Self::StringSplit,
        Self::StringSlice,
        Self::StringLines,
        Self::StringReplace,
        Self::MathSqrt,
        Self::MathPow,
        Self::ArrayLen,
        Self::ArrayFirst,
        Self::ArrayLast,
        Self::ObjectKeys,
        Self::ObjectValues,
        Self::ObjectLen,
        Self::ArrayDistinct,
        Self::ArraySort,
        Self::ArrayReverse,
        Self::ArrayFlatten,
        Self::ArrayJoin,
        Self::ArraySlice,
        Self::MathAbs,
        Self::MathFloor,
        Self::MathCeil,
        Self::MathRound,
        Self::TimeNow,
        Self::TimeYear,
        Self::TimeMonth,
        Self::TimeDay,
        Self::TimeHour,
        Self::TimeMinute,
        Self::TimeSecond,
        Self::TimeUnix,
        Self::TimeFromUnix,
        Self::RandUuid,
        Self::TypeOf,
        Self::TypeBool,
        Self::TypeInt,
        Self::TypeFloat,
        Self::TypeString,
        Self::TypeDatetime,
        Self::TypeUuid,
        Self::VectorCosine,
        Self::VectorEuclidean,
        Self::VectorDot,
        Self::SearchScore,
        Self::SearchHighlight,
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
        Self::CryptoSha256,
        Self::CryptoSha512,
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
            Self::StringSplit => "string::split",
            Self::StringSlice => "string::slice",
            Self::StringLines => "string::lines",
            Self::StringReplace => "string::replace",
            Self::MathSqrt => "math::sqrt",
            Self::MathPow => "math::pow",
            Self::ArrayLen => "array::len",
            Self::ArrayFirst => "array::first",
            Self::ArrayLast => "array::last",
            Self::ObjectKeys => "object::keys",
            Self::ObjectValues => "object::values",
            Self::ObjectLen => "object::len",
            Self::ArrayDistinct => "array::distinct",
            Self::ArraySort => "array::sort",
            Self::ArrayReverse => "array::reverse",
            Self::ArrayFlatten => "array::flatten",
            Self::ArrayJoin => "array::join",
            Self::ArraySlice => "array::slice",
            Self::MathAbs => "math::abs",
            Self::MathFloor => "math::floor",
            Self::MathCeil => "math::ceil",
            Self::MathRound => "math::round",
            Self::TimeNow => "time::now",
            Self::TimeYear => "time::year",
            Self::TimeMonth => "time::month",
            Self::TimeDay => "time::day",
            Self::TimeHour => "time::hour",
            Self::TimeMinute => "time::minute",
            Self::TimeSecond => "time::second",
            Self::TimeUnix => "time::unix",
            Self::TimeFromUnix => "time::from_unix",
            Self::RandUuid => "rand::uuid",
            Self::TypeOf => "type::of",
            Self::TypeBool => "type::bool",
            Self::TypeInt => "type::int",
            Self::TypeFloat => "type::float",
            Self::TypeString => "type::string",
            Self::TypeDatetime => "type::datetime",
            Self::TypeUuid => "type::uuid",
            Self::VectorCosine => "vector::cosine",
            Self::VectorEuclidean => "vector::euclidean",
            Self::VectorDot => "vector::dot",
            Self::SearchScore => "search::score",
            Self::SearchHighlight => "search::highlight",
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
            Self::CryptoSha256 => "crypto::sha256",
            Self::CryptoSha512 => "crypto::sha512",
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
            Self::TimeNow | Self::RandUuid => 0,
            Self::StringLen
            | Self::StringLower
            | Self::StringUpper
            | Self::StringTrim
            | Self::ArrayLen
            | Self::ArrayFirst
            | Self::ArrayLast
            | Self::MathSqrt
            | Self::ObjectKeys
            | Self::ObjectValues
            | Self::ObjectLen
            | Self::ArrayDistinct
            | Self::ArraySort
            | Self::ArrayReverse
            | Self::ArrayFlatten
            | Self::MathAbs
            | Self::MathFloor
            | Self::MathCeil
            | Self::MathRound
            | Self::TimeYear
            | Self::TimeMonth
            | Self::TimeDay
            | Self::TimeHour
            | Self::TimeMinute
            | Self::TimeSecond
            | Self::TimeUnix
            | Self::TimeFromUnix
            | Self::TypeOf
            | Self::TypeBool
            | Self::TypeInt
            | Self::TypeFloat
            | Self::TypeString
            | Self::TypeDatetime
            | Self::TypeUuid
            | Self::GeoArea
            | Self::CryptoSha256
            | Self::SearchHighlight
            | Self::CryptoSha512 => 1,
            Self::StringConcat
            | Self::StringSplit
            | Self::MathPow
            | Self::ArrayJoin
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
            Self::ArraySlice | Self::StringSlice | Self::StringLines | Self::StringReplace => 3,
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
    /// [`Purity::PerStatement`] and [`Purity::PerCall`] are each a category of
    /// one. The tests below write both memberships down as sets, so the next
    /// function that must be asked again for every record despite reading none
    /// cannot be added without somebody reading what folding would do to it —
    /// which is how `rand::uuid` came to be classified before it was evaluated.
    #[must_use]
    pub const fn purity(self) -> Purity {
        match self {
            // The clock moves while a statement runs; the statement should not
            // see it move.
            Self::TimeNow => Purity::PerStatement,
            // The one function the fold may not touch. Reading no record makes
            // it *look* constant, and a `SELECT rand::uuid() AS id` evaluated
            // once above the records hands every row the same id.
            Self::RandUuid => Purity::PerCall,
            Self::StringLen
            | Self::StringLower
            | Self::StringUpper
            | Self::StringTrim
            | Self::StringConcat
            | Self::StringSplit
            | Self::StringSlice
            | Self::StringLines
            | Self::StringReplace
            | Self::MathSqrt
            | Self::MathPow
            | Self::ArrayLen
            | Self::ArrayFirst
            | Self::ArrayLast
            | Self::ObjectKeys
            | Self::ObjectValues
            | Self::ObjectLen
            | Self::ArrayDistinct
            | Self::ArraySort
            | Self::ArrayReverse
            | Self::ArrayFlatten
            | Self::ArrayJoin
            | Self::ArraySlice
            | Self::MathAbs
            | Self::MathFloor
            | Self::MathCeil
            | Self::MathRound
            | Self::TimeYear
            | Self::TimeMonth
            | Self::TimeDay
            | Self::TimeHour
            | Self::TimeMinute
            | Self::TimeSecond
            | Self::TimeUnix
            | Self::TimeFromUnix
            | Self::TypeOf
            | Self::TypeBool
            | Self::TypeInt
            | Self::TypeFloat
            | Self::TypeString
            | Self::TypeDatetime
            | Self::TypeUuid
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
            | Self::GeoArea
            | Self::CryptoSha256
            | Self::SearchHighlight
            | Self::CryptoSha512 => Purity::Pure,
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
    /// The `type::` **casts** are deliberately not in the list, though
    /// [`Function::TypeOf`] beside them is. `type::of` asks what a value is, and
    /// an absence has an answer to that. A cast asks for the value *as* a kind,
    /// and there is no `int` that a missing field is — so the general rule
    /// applies and the answer is `none`, which is what lets `type::int(price)`
    /// run over a table where some records have no price.
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
                | Self::SearchHighlight
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
    /// be *classified*, but a classification is a claim about how the function
    /// must be evaluated, and the two can be written apart: a function put here
    /// wrongly is folded to one value per statement and nothing says so.
    ///
    /// So the set is written down. Adding a member fails this test, and the
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

    /// The whole membership of [`Purity::PerCall`], asserted the same way.
    ///
    /// This is the list `plan::fold` refuses to evaluate above the records, so
    /// widening it makes a read slower and narrowing it makes one **wrong** —
    /// asymmetric, and the direction that costs correctness is the one a
    /// classification typo takes silently.
    #[test]
    fn the_functions_the_fold_may_not_touch_are_exactly_the_one_that_generates() {
        let afresh: Vec<&str> = Function::ALL
            .iter()
            .filter(|function| function.purity() == Purity::PerCall)
            .map(|function| function.spelling())
            .collect();
        assert_eq!(afresh, ["rand::uuid"]);
    }

    #[test]
    fn every_function_is_classified_and_only_the_clock_and_the_generator_are_not_pure() {
        for function in Function::ALL {
            let expected = match function {
                Function::TimeNow => Purity::PerStatement,
                Function::RandUuid => Purity::PerCall,
                _ => Purity::Pure,
            };
            assert_eq!(function.purity(), expected, "{function} is misclassified");
        }
    }

    /// Every cast names a kind a field can be declared as, spelled identically.
    ///
    /// The decision this holds in place: a cast and a `DEFINE FIELD … TYPE` say
    /// the same word for the same kind. Two vocabularies for one type system is
    /// the sort of thing that reads fine in each file and forces every author to
    /// remember which side of the language they are on — `type::integer(x)` into
    /// a field declared `int`, and no error anywhere to point at it.
    ///
    /// `type::of` is excluded because it is not a cast: it answers *about* a
    /// value rather than producing one of a kind.
    #[test]
    fn every_cast_spells_its_kind_the_way_a_field_declaration_does() {
        let casts: Vec<&str> = Function::ALL
            .iter()
            .filter_map(|function| {
                function
                    .spelling()
                    .strip_prefix("type::")
                    .filter(|name| *name != "of")
            })
            .collect();
        assert_eq!(
            casts,
            ["bool", "int", "float", "string", "datetime", "uuid"],
            "the cast set moved"
        );
        for name in casts {
            assert!(
                tessari_types::FieldKind::parse(name).is_some_and(|kind| kind.name() == name),
                "`type::{name}` is not the spelling a field declaration uses"
            );
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
