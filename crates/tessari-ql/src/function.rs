//! The functions the language has, and how many arguments each one takes.
//!
//! # What earns a place
//!
//! **A function is here when it cannot be expressed by what the language already
//! has.** That is why there is no `array::contains` (`CONTAINS` says it) and no
//! `is_none` (`= NONE` says it). The rule keeps the surface from growing by
//! association and gives a reviewer one question to ask about any addition.
//!
//! `string::contains` was refused on the same grounds — `LIKE '%x%'` says it —
//! and that holds for a **literal** needle only. A needle arriving as a value
//! may carry `%` or `_`, and the escape that would fix it cannot be applied to
//! text the statement has never seen, so the function was admitted and the
//! specification records the reversal. This paragraph stands where the refusal
//! did, because the refusal outlived the decision that reversed it.
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

mod names;
mod signature;
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
    /// `string::concat(a, b, …)` — the strings in order; two or more.
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
    /// `time::day_of_week(instant)` — the ISO-8601 day of the week, Monday 1
    /// through Sunday 7.
    TimeDayOfWeek,
    /// `time::day_of_year(instant)` — the day of the year, 1 through 366.
    TimeDayOfYear,
    /// `time::week(instant)` — the ISO-8601 week number, 1 through 53. A week
    /// belongs to the year holding its Thursday, so the first days of January
    /// can read 52 or 53 and the last of December can read 1.
    TimeWeek,
    /// `time::quarter(instant)` — the quarter of the year, 1 through 4.
    TimeQuarter,
    /// `time::start_of_day(instant)` — midnight of the instant's own day.
    TimeStartOfDay,
    /// `time::start_of_week(instant)` — midnight of the week's Monday,
    /// ISO-8601 like [`Function::TimeDayOfWeek`].
    TimeStartOfWeek,
    /// `time::start_of_month(instant)` — midnight of the month's first day.
    TimeStartOfMonth,
    /// `time::start_of_quarter(instant)` — midnight of the quarter's first day.
    TimeStartOfQuarter,
    /// `time::start_of_year(instant)` — midnight of the year's first day.
    TimeStartOfYear,
    /// `time::unix(instant)` — whole seconds since the epoch.
    TimeUnix,
    /// `time::from_unix(seconds)` — the instant a second count names.
    TimeFromUnix,
    /// `duration::from_secs(seconds)` — the span a number of seconds names,
    /// whole or fractional (ADR-0124 D4).
    DurationFromSecs,
    /// `rand::uuid()` — a fresh version-4 identifier, once per call.
    RandUuid,
    /// `type::of(value)` — the type's name, as §3 spells it.
    TypeOf,
    /// `state::of(<fold>)` — the group's STATE instead of the fold's finish
    /// (G102 S11). Legal only over a bare fold in a grouping projection; the
    /// grouping machinery answers it, and any other position refuses.
    StateOf,
    /// `state::finish(state)` — the fold's own answer out of a stored state.
    StateFinish,
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
    /// `vector::manhattan(a, b)` — the sum of the absolute differences, the
    /// distance a grid allows.
    VectorManhattan,
    /// `vector::hamming(a, b)` — how many positions hold different numbers,
    /// as an integer: the one distance in the family that is exactly
    /// countable.
    VectorHamming,
    /// `vector::jaccard(a, b)` — the Jaccard distance between the supports:
    /// one minus the share of positions where both are nonzero out of the
    /// positions where either is. Two zero vectors share no support and
    /// answer no distance, by the zero-vector rule `vector::cosine` keeps.
    VectorJaccard,
    /// `search::score(field, 'query')` — how well this record answers the query,
    /// measured against the collection the field's search index summarises.
    SearchScore,
    /// `search::explain(field, 'query')` — the score `search::score` answers,
    /// with the collection's numbers and what each asked word contributed to it.
    SearchExplain,
    /// `search::highlight(field)` — where in this record's text the read's own
    /// query matched, as `{ start, end }` byte ranges.
    ///
    /// It takes the **field alone**. The query comes from what the statement
    /// asked of that field, so a highlight cannot disagree with the filter that
    /// selected the record — see the session's `search::highlight` for why a
    /// second copy of the query is the failure this signature avoids.
    SearchHighlight,
    /// `search::ranks()` — where this record came in each branch of the fused
    /// order that answered it (`ORDER BY FUSE`), `none` where a branch did not
    /// place it within its depth.
    SearchRanks,
    /// `search::table_name()` — the name of the table a `FROM SEARCH` record came from
    /// (ADR-0105).
    SearchTable,
    /// `search::snippet()` — the best window of a `FROM SEARCH` record's
    /// `SNIPPET` fields, as `{ field, start, end }` byte offsets.
    SearchSnippet,
    /// `time::bucket(instant, 1h)` — the start of the window that instant is in.
    TimeBucket,
    /// `time::format(instant, pattern)` — the instant as text, in UTC (the
    /// accessors' rule), by a strftime-shaped pattern: `%Y %m %d %H %M %S`
    /// zero-padded, `%f` the nanosecond remainder as nine digits, `%%` a
    /// literal percent; any other character copies verbatim. A token outside
    /// the set is refused rather than copied — a `%q` passed through would
    /// read as a formatted answer.
    TimeFormat,
    /// `math::log2(number)` — the base-2 logarithm, `NONE` at zero or below.
    MathLog2,
    /// `math::log10(number)` — the base-10 logarithm, `NONE` at zero or below.
    MathLog10,
    /// `math::exp2(number)` — two to the power.
    MathExp2,
    /// `math::cbrt(number)` — the cube root, negative for a negative.
    MathCbrt,
    /// `math::sin(number)`, `math::cos(number)`, `math::tan(number)` — the
    /// angle in radians.
    MathSin,
    /// See [`Function::MathSin`].
    MathCos,
    /// See [`Function::MathSin`].
    MathTan,
    /// `math::asin(number)`, `math::acos(number)` — `NONE` outside −1 to 1.
    MathAsin,
    /// See [`Function::MathAsin`].
    MathAcos,
    /// `math::atan(number)` — the angle whose tangent this is.
    MathAtan,
    /// `math::atan2(y, x)` — the angle of the point, in the right quadrant.
    MathAtan2,
    /// `math::hypot(a, b)` — the hypotenuse, without the overflow a square
    /// and a root would risk.
    MathHypot,
    /// `math::degrees(radians)` — radians as degrees.
    MathDegrees,
    /// `math::radians(degrees)` — degrees as radians.
    MathRadians,
    /// `math::pi()` — π as a float.
    MathPi,
    /// `math::e()` — e as a float.
    MathE,
    /// `string::repeat(text, n)` — the text `n` times.
    StringRepeat,
    /// `string::pad_left(text, n, fill)` — the text brought to `n` characters
    /// with `fill` on the left; a fill that is not one character is refused.
    StringPadLeft,
    /// `string::pad_right(text, n, fill)` — the same on the right.
    StringPadRight,
    /// `string::edit_distance(a, b)` — the fewest single-character edits
    /// turning one text into the other, characters rather than bytes.
    StringEditDistance,
    /// `url::scheme(url)` — the scheme of a URL, lowercased; `NONE` when the
    /// text is not a URL.
    UrlScheme,
    /// `url::host(url)` — the host of a URL; `NONE` when the text is not a
    /// URL or names no host.
    UrlHost,
    /// `url::port(url)` — the port a URL names, as an integer; `NONE` when it
    /// names none.
    UrlPort,
    /// `url::path(url)` — the path of a URL, `''` when it names none.
    UrlPath,
    /// `url::query(url)` — the query of a URL without its `?`; `NONE` when it
    /// names none.
    UrlQuery,
    /// `url::fragment(url)` — the fragment of a URL without its `#`; `NONE`
    /// when it names none.
    UrlFragment,
    /// `url::decode(text)` — the text with its percent-escapes read; `NONE`
    /// when an escape is not two hex digits.
    UrlDecode,
    /// `url::encode(text)` — the text with every byte outside the unreserved
    /// set percent-escaped, uppercase hex.
    UrlEncode,
    /// `ip::is_v4(text)` — whether the text parses as an IPv4 address.
    IpIsV4,
    /// `ip::is_v6(text)` — whether the text parses as an IPv6 address.
    IpIsV6,
    /// `ip::in_range(ip, cidr)` — whether the address is inside the range;
    /// refused when either side does not parse.
    IpInRange,
    /// `array::compact(items)` — the elements that are present, `NONE`s gone.
    ArrayCompact,
    /// `array::count(items, value)` — how many elements equal the value.
    ArrayCount,
    /// `array::range(start, end)` — the integers from `start` up to `end`,
    /// `end` not included; an empty range is `[]`.
    ArrayRange,
    /// `rand::uuid7()` — a fresh version-7 identifier: time-ordered, once
    /// per call.
    RandUuid7,
    /// `uuid::instant(id)` — the instant a version-7 identifier was minted
    /// at, to the millisecond; `NONE` for any other version.
    UuidInstant,
    /// `uuid::version(id)` — which version the identifier's own nibble says
    /// it is, as an integer. A fact about the bytes rather than a validity
    /// judgement, so any version answers; `NONE` for a value that is no
    /// UUID, by [`Function::UuidInstant`]'s rule.
    UuidVersion,
    /// `series::outliers_tukey(values)` — the indices whose numbers fall
    /// outside the Tukey fences, 1.5 interquartile ranges past either
    /// quartile.
    SeriesOutliersTukey,
    /// `series::period_detect(values)` — the dominant period of the series in
    /// samples, read off the strongest non-DC Fourier component; `NONE` for
    /// fewer than four samples.
    SeriesPeriodDetect,
    /// `series::decompose_stl(values, period)` — classical seasonal
    /// decomposition: `{ trend, seasonal, residual }`, each an array the
    /// length of the series with `NONE` where a part is undefined (the
    /// trend's edges).
    SeriesDecomposeStl,
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
    /// `geo::distance(a, b)` — how far apart two shapes are along the
    /// ellipsoid, in **metres**, measured to the nearest point of each. At least
    /// one argument must be a position; there is no distance between two larger
    /// shapes yet.
    GeoDistance,
    /// `geo::area(shape)` — how much ground a shape covers, in **square
    /// metres**. Zero for anything with no interior.
    GeoArea,
    /// `geo::cell(position, level)` — the spatial index's cell holding the
    /// position at that level of subdivision, as a polygon: a key to group by
    /// that draws itself.
    GeoCell,
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
    /// `crypto::md5(text)` — the MD5 digest, as thirty-two lowercase
    /// hexadecimal characters.
    ///
    /// **A checksum, and broken as anything else.** Collisions in MD5 are
    /// producible on a laptop, so it must never decide whether two things are
    /// the same when somebody might want them to appear so. It is in the
    /// language because a store interoperates: an ETag, a legacy row key, a
    /// content id computed by something older than this database. Refusing to
    /// spell it would not make any of those safer — it would make them
    /// unreachable from here, which sends the caller to a place with no grant
    /// check at all.
    ///
    /// Everything [`Self::CryptoSha256`] says about text in, text out and about
    /// passwords holds here unchanged.
    CryptoMd5,
    /// `crypto::sha1(text)` — the SHA-1 digest, as forty lowercase hexadecimal
    /// characters.
    ///
    /// A checksum on the same terms as [`Self::CryptoMd5`]: collisions have
    /// been produced, so it decides nothing an adversary has an interest in,
    /// and it exists because git object ids, older ETags and a great deal of
    /// installed software speak it.
    CryptoSha1,
    /// `encoding::base64(bytes)` — the bytes as standard base64 text, padded.
    ///
    /// RFC 4648 §4 with the `+/` alphabet and `=` padding, which is what a
    /// caller pasting the result into anything else will be understood to mean.
    /// The URL-safe alphabet is a different function and is not written until
    /// somebody needs it.
    EncodingBase64,
    /// `encoding::base64_decode(text)` — the bytes that text encodes, or `NONE`
    /// when it encodes none.
    ///
    /// `NONE` rather than a refusal, and for the reason every cast answers that
    /// way: this is a question about a **value**, not about a kind, and a table
    /// holding one unparseable row should narrow rather than become unreadable.
    EncodingBase64Decode,
    /// `encoding::hex(bytes)` — the bytes as lowercase hexadecimal text.
    EncodingHex,
    /// `encoding::hex_decode(text)` — the bytes that text spells, or `NONE`.
    ///
    /// Either case of letter is read; an odd number of characters or anything
    /// outside `0-9a-fA-F` answers `NONE`, on [`Self::EncodingBase64Decode`]'s
    /// reading.
    EncodingHexDecode,
    /// `json::parse(text)` — the value that JSON text spells, or `NONE` when it
    /// spells none (ADR-0116).
    ///
    /// The one JSON reader this store has, so a script reads a document exactly
    /// as a stream consumer does: a number with no fraction is an integer, a
    /// duplicate key keeps its last value, `null` is `NULL` and not `NONE`, and
    /// nesting is bounded. Text that is not one JSON value answers `NONE`, on
    /// [`Self::EncodingBase64Decode`]'s reading — a question about a value.
    JsonParse,
    /// `json::encode(value)` — the value as compact JSON text (ADR-0116).
    ///
    /// The mapping the HTTP surface writes, so what this answers is what
    /// `POST /script` shows: object keys in name order, which is the order the
    /// store holds them in (a document's insertion order is not kept); a field
    /// holding `NONE` is left out; a decimal is quoted so it does not become a
    /// double; a record reference is written by its table's name.
    JsonEncode,
    /// `string::starts_with(text, prefix)`
    ///
    /// Not a spelling of `LIKE`, and that is the point: a `LIKE` pattern is a
    /// pattern, so a prefix holding `%` or `_` cannot be written as one without
    /// escaping it — and a caller who forgets quietly matches more than they
    /// meant. This takes a **value**.
    StringStartsWith,
    /// `string::ends_with(text, suffix)` — [`Self::StringStartsWith`]'s other end.
    StringEndsWith,
    /// `string::contains(text, needle)` — anywhere in the text.
    ///
    /// The `CONTAINS` operator is about an array holding a value; this is about
    /// text holding text, and they are different questions that would be one
    /// word if this were spelled as an operator.
    StringContains,
    /// `string::index_of(text, needle)` — where it first occurs, counted in
    /// characters, or `NONE`.
    ///
    /// `NONE` and never `-1`. A sentinel that is also a number travels through
    /// arithmetic and an ordering as though it meant something.
    StringIndexOf,
    /// `string::reverse(text)` — the characters back to front.
    ///
    /// Characters, as [`Self::StringLen`] counts them. Reversing the bytes of
    /// UTF-8 does not produce text.
    StringReverse,
    /// `string::trim_start(text)` — whitespace from the front only.
    StringTrimStart,
    /// `string::trim_end(text)` — whitespace from the end only.
    StringTrimEnd,
    /// `math::min(a, b)` — the smaller of two numbers.
    ///
    /// Two numbers, not an array and not a column: `min` over rows is an
    /// aggregate and already exists, and confusing the two is how a projection
    /// silently folds a table.
    MathMin,
    /// `math::max(a, b)` — the larger, on [`Self::MathMin`]'s terms.
    MathMax,
    /// `math::sign(number)` — `-1`, `0` or `1`.
    MathSign,
    /// `math::trunc(number)` — the whole part, toward zero.
    ///
    /// Distinct from [`Self::MathFloor`] for negatives, which is the only place
    /// the two differ and the only place anybody is surprised.
    MathTrunc,
    /// `math::ln(number)` — the natural logarithm, or `NONE` at zero and below.
    ///
    /// `NONE` rather than `-∞` or a NaN, on [`Self::MathSqrt`]'s reading: a
    /// value that compares false against everything travels through a filter
    /// and an ordering without saying anything.
    MathLn,
    /// `math::exp(number)` — `e` raised to it.
    MathExp,
    /// `array::concat(a, b)` — one array holding both, in order.
    ArrayConcat,
    /// `array::append(items, value)` — the array with one more value at the end.
    ///
    /// Separate from [`Self::ArrayConcat`] because appending an **array** and
    /// appending *to* an array are different intentions, and one function doing
    /// both decides which by inspecting the argument's kind — which is how a
    /// caller appending a genuine array of two ends up with two elements.
    ArrayAppend,
    /// `array::index_of(items, value)` — the first position holding it, or
    /// `NONE`, on [`Self::StringIndexOf`]'s reading.
    ArrayIndexOf,
    /// `array::min(items)` — the smallest, in the value system's order, or
    /// `NONE` over nothing.
    ///
    /// This folds **one array in one record**. The `min` aggregate folds a
    /// column across records. Both are wanted and neither can be written as the
    /// other.
    ArrayMin,
    /// `array::max(items)` — the largest, on [`Self::ArrayMin`]'s terms.
    ArrayMax,
    /// `array::sum(items)` — the numbers added; over nothing, zero.
    ///
    /// Zero over an empty array, which is what the `sum` aggregate answers over
    /// no rows, so the two agree where they meet.
    ArraySum,
    /// `object::entries(o)` — one two-element array per field, name first.
    ///
    /// The inverse of reading [`Self::ObjectKeys`] and [`Self::ObjectValues`]
    /// separately, which the language could not zip back together.
    ObjectEntries,
    /// `object::has(o, name)` — whether the field is there at all.
    ///
    /// Different from comparing the field against `NONE`: a field explicitly
    /// holding `none` is present, and this is the only way to tell the two
    /// apart.
    ObjectHas,
    /// `object::merge(a, b)` — both objects' fields, `b` winning a collision.
    ObjectMerge,
    /// `object::get(o, name)` — the field a name held in a value names.
    ///
    /// `o.field` takes a written name and is part of the stored path type, so a
    /// name arriving as a parameter could not be asked for (ADR-0135 D2).
    ObjectGet,
    /// `array::at(items, i)` — the element at a position held in a value, a
    /// negative one counted from the back as `LIST SET AT` counts it.
    ArrayAt,
    /// `set::len(s)` — how many elements; a ranking counts its members.
    SetLen,
    /// `set::union(a, b)` — every member of either set of members.
    SetUnion,
    /// `set::intersect(a, b)` — the members in both.
    SetIntersect,
    /// `set::difference(a, b)` — the members of `a` that are not in `b`.
    SetDifference,
    /// `scores::score(r, member)` — a ranked member's score.
    ScoresScore,
    /// `scores::rank(r, member)` — a member's place from the lowest score, from 0.
    ScoresRank,
    /// `scores::range(r, start, stop)` — the members at those places, both kept,
    /// either counted from the highest when negative.
    ScoresRange,
    /// `scores::between(r, low, high)` — the members whose score is in the
    /// bounds, both kept; an absent bound leaves that side open.
    ScoresBetween,
    /// `scores::union(a, b)` — a ranking of every member of either, scores
    /// added where a member is in both.
    ScoresUnion,
    /// `scores::intersect(a, b)` — a ranking of the members in both, their
    /// scores added.
    ScoresIntersect,
    /// `session::context()` — `{ node, namespace, database }`: the node this
    /// session is talking to and the tenancy the session has selected, `null`
    /// where nothing is. What a client following a redirect checks on arrival
    /// and replays from where it left (G051 SG3).
    SessionContext,
    /// `ai::generate(prompt)` — one call to the namespace's declared AI
    /// provider, answered with the reply text. The call leaves the store: a
    /// token-bucket budget bounds it, a stated timeout ends it, and the
    /// provider's key is read from its vault at call time and never stored
    /// or answered (G102 C8).
    AiGenerate,
    /// `ai::summarize(text)` — a bounded summarising prompt over the same
    /// provider call, answered with the reply text.
    AiSummarize,
    /// `ai::classify(text, labels)` — a classifying prompt over the same
    /// provider call; the reply is validated against the declared labels and
    /// answers the one it names, or `NONE` when it names none of them.
    AiClassify,
    /// `ai::extract(text, field)` — an extracting prompt over the same
    /// provider call: one named field's value out of the text.
    AiExtract,
    /// `ai::translate(text, language)` — a translating prompt over the same
    /// provider call, answered with the reply text.
    AiTranslate,
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling())
    }
}

#[cfg(test)]
mod tests;
