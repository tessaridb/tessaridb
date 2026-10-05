use super::*;

/// The functions, declared once.
///
/// Expands to [`Function::ALL`] and to the exhaustive match in
/// [`Function::spelling`], from one set of rows. A variant missing from the rows
/// fails that match's exhaustiveness check, which is a compile error — and that
/// is the whole of what this macro is for.
///
/// `ALL` used to be a hand-written list *beside* the match, which is two
/// declarations of one table with the compiler checking one of them. Every
/// function ratchet iterates `ALL` — the two purity memberships, the
/// classification sweep, the cast vocabulary, the corpus coverage in
/// `tessari-conformance` and the documentation coverage — so a variant absent
/// from it was outside all of their universes at once rather than merely
/// unchecked by them. And [`Function::parse`] searches `ALL`, so the function
/// would not have been parseable either: the language would have refused a
/// correctly written call to a function it has.
///
/// The same repair for the same reason is `tessari-conformance`'s `forms!`,
/// where seven statement forms had already drifted through the gap before it
/// was closed.
macro_rules! functions {
    ($($variant:ident => $spelling:literal),+ $(,)?) => {
        /// Every function, so that a listing cannot drift from the set.
        pub const ALL: &'static [Self] = &[$(Self::$variant),+];

        /// How the function is written, group and name together.
        #[must_use]
        pub const fn spelling(self) -> &'static str {
            match self {
                $(Self::$variant => $spelling,)+
            }
        }
    };
}

impl Function {
    functions! {
        StringLen => "string::len",
        StringLower => "string::lower",
        StringUpper => "string::upper",
        StringTrim => "string::trim",
        StringConcat => "string::concat",
        StringSplit => "string::split",
        StringSlice => "string::slice",
        StringLines => "string::lines",
        StringReplace => "string::replace",
        MathSqrt => "math::sqrt",
        MathPow => "math::pow",
        ArrayLen => "array::len",
        ArrayFirst => "array::first",
        ArrayLast => "array::last",
        ObjectKeys => "object::keys",
        ObjectValues => "object::values",
        ObjectLen => "object::len",
        ArrayDistinct => "array::distinct",
        ArraySort => "array::sort",
        ArrayReverse => "array::reverse",
        ArrayFlatten => "array::flatten",
        ArrayJoin => "array::join",
        ArraySlice => "array::slice",
        MathAbs => "math::abs",
        MathFloor => "math::floor",
        MathCeil => "math::ceil",
        MathRound => "math::round",
        TimeNow => "time::now",
        TimeYear => "time::year",
        TimeMonth => "time::month",
        TimeDay => "time::day",
        TimeHour => "time::hour",
        TimeMinute => "time::minute",
        TimeSecond => "time::second",
        TimeUnix => "time::unix",
        TimeFromUnix => "time::from_unix",
        RandUuid => "rand::uuid",
        TypeOf => "type::of",
        TypeBool => "type::bool",
        TypeInt => "type::int",
        TypeFloat => "type::float",
        TypeString => "type::string",
        TypeDatetime => "type::datetime",
        TypeUuid => "type::uuid",
        VectorCosine => "vector::cosine",
        VectorEuclidean => "vector::euclidean",
        VectorDot => "vector::dot",
        SearchScore => "search::score",
        SearchExplain => "search::explain",
        SearchHighlight => "search::highlight",
        SearchRanks => "search::ranks",
        SearchTable => "search::table_name",
        SearchSnippet => "search::snippet",
        TimeBucket => "time::bucket",
        GeoIntersects => "geo::intersects",
        GeoDisjoint => "geo::disjoint",
        GeoCovers => "geo::covers",
        GeoCoveredBy => "geo::covered_by",
        GeoContains => "geo::contains",
        GeoWithin => "geo::within",
        GeoEquals => "geo::equals",
        GeoTouches => "geo::touches",
        GeoDistance => "geo::distance",
        GeoArea => "geo::area",
        GeoCell => "geo::cell",
        CryptoSha256 => "crypto::sha256",
        CryptoSha512 => "crypto::sha512",
        CryptoMd5 => "crypto::md5",
        CryptoSha1 => "crypto::sha1",
        EncodingBase64 => "encoding::base64",
        EncodingBase64Decode => "encoding::base64_decode",
        EncodingHex => "encoding::hex",
        EncodingHexDecode => "encoding::hex_decode",
        JsonParse => "json::parse",
        JsonEncode => "json::encode",
        StringStartsWith => "string::starts_with",
        StringEndsWith => "string::ends_with",
        StringContains => "string::contains",
        StringIndexOf => "string::index_of",
        StringReverse => "string::reverse",
        StringTrimStart => "string::trim_start",
        StringTrimEnd => "string::trim_end",
        MathMin => "math::min",
        MathMax => "math::max",
        MathSign => "math::sign",
        MathTrunc => "math::trunc",
        MathLn => "math::ln",
        MathExp => "math::exp",
        ArrayConcat => "array::concat",
        ArrayAppend => "array::append",
        ArrayIndexOf => "array::index_of",
        ArrayMin => "array::min",
        ArrayMax => "array::max",
        ArraySum => "array::sum",
        ObjectEntries => "object::entries",
        ObjectHas => "object::has",
        ObjectMerge => "object::merge",
        SessionContext => "session::context",
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
    ///   one. [`Function::SearchExplain`] answers the explanation of that zero.
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
                | Self::SearchExplain
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
