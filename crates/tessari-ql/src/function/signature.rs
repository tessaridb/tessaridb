//! How many arguments each function takes, and whether its answer can change.

use super::{Function, Purity};

impl Function {
    /// How many arguments it takes.
    /// Whether a call with this many arguments is well formed.
    ///
    /// The declared [`Self::arity`], and one exception: `search::score()` with
    /// no argument is the score a `FROM SEARCH` read gave its record
    /// (ADR-0105), where the two-argument form measures one field.
    #[must_use]
    pub const fn accepts(self, count: usize) -> bool {
        count == self.arity() || matches!((self, count), (Self::SearchScore, 0))
    }

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
            Self::TimeNow
            | Self::RandUuid
            | Self::SearchRanks
            | Self::SearchTable
            | Self::SearchSnippet
            | Self::SessionContext => 0,
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
            | Self::CryptoMd5
            | Self::CryptoSha1
            | Self::EncodingBase64
            | Self::EncodingBase64Decode
            | Self::EncodingHex
            | Self::EncodingHexDecode
            | Self::JsonParse
            | Self::JsonEncode
            | Self::StringReverse
            | Self::StringTrimStart
            | Self::StringTrimEnd
            | Self::MathSign
            | Self::MathTrunc
            | Self::MathLn
            | Self::MathExp
            | Self::ArrayMin
            | Self::ArrayMax
            | Self::ArraySum
            | Self::ObjectEntries
            | Self::CryptoSha512 => 1,
            Self::StringConcat
            | Self::StringSplit
            | Self::MathPow
            | Self::ArrayJoin
            | Self::VectorCosine
            | Self::VectorEuclidean
            | Self::VectorDot
            | Self::SearchScore
            | Self::SearchExplain
            | Self::TimeBucket
            | Self::GeoIntersects
            | Self::GeoDisjoint
            | Self::GeoCovers
            | Self::GeoCoveredBy
            | Self::GeoContains
            | Self::GeoWithin
            | Self::GeoEquals
            | Self::GeoTouches
            | Self::StringStartsWith
            | Self::StringEndsWith
            | Self::StringContains
            | Self::StringIndexOf
            | Self::MathMin
            | Self::MathMax
            | Self::ArrayConcat
            | Self::ArrayAppend
            | Self::ArrayIndexOf
            | Self::ObjectHas
            | Self::ObjectMerge
            | Self::GeoCell
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
            // The node and the session's tenancy cannot change while one
            // statement runs: a `USE` is a statement of its own.
            Self::SessionContext => Purity::PerStatement,
            // The one function the fold may not touch. Reading no record makes
            // it *look* constant, and a `SELECT rand::uuid() AS id` evaluated
            // once above the records hands every row the same id.
            Self::RandUuid => Purity::PerCall,
            // A record's ranks are the fusion's, not a function of anything the
            // call is given: folded above the records it would answer one row's
            // ranks for every row.
            Self::SearchRanks => Purity::PerCall,
            // A search's record is not an argument either: what the read ranked
            // it, and where it came from, are answers per record (ADR-0105) —
            // and `search::score()` takes no argument there, so folding it as a
            // constant would hand every row the first one's score.
            Self::SearchTable | Self::SearchSnippet | Self::SearchScore => Purity::PerCall,
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
            | Self::SearchExplain
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
            | Self::GeoCell
            | Self::CryptoSha256
            | Self::SearchHighlight
            | Self::CryptoMd5
            | Self::CryptoSha1
            | Self::EncodingBase64
            | Self::EncodingBase64Decode
            | Self::EncodingHex
            | Self::EncodingHexDecode
            | Self::JsonParse
            | Self::JsonEncode
            | Self::StringStartsWith
            | Self::StringEndsWith
            | Self::StringContains
            | Self::StringIndexOf
            | Self::StringReverse
            | Self::StringTrimStart
            | Self::StringTrimEnd
            | Self::MathMin
            | Self::MathMax
            | Self::MathSign
            | Self::MathTrunc
            | Self::MathLn
            | Self::MathExp
            | Self::ArrayConcat
            | Self::ArrayAppend
            | Self::ArrayIndexOf
            | Self::ArrayMin
            | Self::ArrayMax
            | Self::ArraySum
            | Self::ObjectEntries
            | Self::ObjectHas
            | Self::ObjectMerge
            | Self::CryptoSha512 => Purity::Pure,
        }
    }
}
