//! What each of the language's functions does, once its arguments are values.
//!
//! Arity was checked when the statement was read (`tessari-ql`'s `Function`
//! knows the set), so what is left here is what each argument *holds* — which
//! nothing could know before a record was in hand.
//!
//! A wrong type names the function, the position, what was wanted and what was
//! there. A message saying only "wrong type" makes the author guess which of
//! three arguments it meant.

use tessari_ql::{Aggregate, Function, Span};
use tessari_types::{Datetime, Number, Value};

use crate::error::{Error, Result};

/// Evaluate a call, with its arguments already values.
///
/// **An absent or null argument answers `none`**, without the function being
/// run. A route that reaches nothing evaluates to `none`, so without this rule a
/// single record missing a field would fail the whole read — and a store that
/// holds documents of differing shapes cannot have a function surface that only
/// works when every record has every field. It is the same rule a filter
/// already applies one level up, carried into the calls.
///
/// The exceptions are the functions that **have** an answer for one, listed by
/// [`Function::answers_for_absence`]: `type::of` asks about a value rather than
/// computing from one, and a distance to something that is not there is
/// unbounded rather than unknown.
pub(crate) fn call(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    if !function.answers_for_absence()
        && arguments
            .iter()
            .any(|value| !value.is_present() || *value == Value::Null)
    {
        return Ok(Value::None);
    }
    match function {
        Function::StringLen => {
            let text = text_at(function, arguments, 0, span)?;
            // Characters, not bytes: a caller asking how long a name is means
            // the name, not its encoding.
            count(text.chars().count(), function, span)
        }
        Function::StringLower => Ok(Value::from(
            text_at(function, arguments, 0, span)?
                .to_lowercase()
                .as_str(),
        )),
        Function::StringUpper => Ok(Value::from(
            text_at(function, arguments, 0, span)?
                .to_uppercase()
                .as_str(),
        )),
        Function::StringTrim => Ok(Value::from(text_at(function, arguments, 0, span)?.trim())),
        // Text in and text out, and the argument check is the one every string
        // function uses — see `crate::digest` for why hashing any value's
        // rendering was refused.
        Function::CryptoSha256 => Ok(crate::digest::sha256(text_at(
            function, arguments, 0, span,
        )?)),
        Function::CryptoSha512 => Ok(crate::digest::sha512(text_at(
            function, arguments, 0, span,
        )?)),
        // On the same terms as the two above, and see `crate::digest` for what
        // "the same terms" leaves out: both of these are checksums and neither
        // decides anything an adversary has an interest in.
        Function::CryptoMd5 => Ok(crate::digest::md5(text_at(function, arguments, 0, span)?)),
        Function::CryptoSha1 => Ok(crate::digest::sha1(text_at(function, arguments, 0, span)?)),
        Function::EncodingBase64 => Ok(crate::encoding::base64(bytes_at(
            function, arguments, 0, span,
        )?)),
        Function::EncodingHex => Ok(crate::encoding::hex(bytes_at(
            function, arguments, 0, span,
        )?)),
        // Text that spells no bytes answers `NONE` rather than failing: the
        // kind was checked above, so what is left is a question about a value,
        // and one unparseable row should narrow a read rather than end it.
        Function::EncodingBase64Decode => Ok(crate::encoding::base64_decode(text_at(
            function, arguments, 0, span,
        )?)
        .map_or(Value::None, Value::Bytes)),
        Function::EncodingHexDecode => Ok(crate::encoding::hex_decode(text_at(
            function, arguments, 0, span,
        )?)
        .map_or(Value::None, Value::Bytes)),
        Function::StringStartsWith => {
            let text = text_at(function, arguments, 0, span)?;
            let prefix = text_at(function, arguments, 1, span)?;
            Ok(Value::Bool(text.starts_with(prefix)))
        }
        Function::StringEndsWith => {
            let text = text_at(function, arguments, 0, span)?;
            let suffix = text_at(function, arguments, 1, span)?;
            Ok(Value::Bool(text.ends_with(suffix)))
        }
        Function::StringContains => {
            let text = text_at(function, arguments, 0, span)?;
            let needle = text_at(function, arguments, 1, span)?;
            Ok(Value::Bool(text.contains(needle)))
        }
        Function::StringIndexOf => {
            let text = text_at(function, arguments, 0, span)?;
            let needle = text_at(function, arguments, 1, span)?;
            // `find` answers a **byte** offset and every other position in this
            // language is a character, so it is converted rather than reported.
            // The two agree on ASCII, which is exactly why the difference would
            // survive a test corpus that never left it.
            match text.find(needle) {
                Some(at) => count(
                    text.get(..at).map_or(0, |before| before.chars().count()),
                    function,
                    span,
                ),
                None => Ok(Value::None),
            }
        }
        Function::StringReverse => Ok(Value::from(
            text_at(function, arguments, 0, span)?
                .chars()
                .rev()
                .collect::<String>()
                .as_str(),
        )),
        Function::StringTrimStart => Ok(Value::from(
            text_at(function, arguments, 0, span)?.trim_start(),
        )),
        Function::StringTrimEnd => Ok(Value::from(
            text_at(function, arguments, 0, span)?.trim_end(),
        )),
        // Two numbers, in the value system's own order, so `math::min` and the
        // `min` aggregate cannot disagree about which of two values is smaller.
        Function::MathMin | Function::MathMax => {
            let first = number_at(function, arguments, 0, span)?;
            let second = number_at(function, arguments, 1, span)?;
            let smaller = function == Function::MathMin;
            let held = if (first <= second) == smaller {
                first
            } else {
                second
            };
            Ok(Value::Number(held.clone()))
        }
        Function::MathSign => {
            let number = number_at(function, arguments, 0, span)?;
            let zero = Number::Integer(0);
            Ok(Value::Number(Number::Integer(match number.cmp(&zero) {
                core::cmp::Ordering::Less => -1,
                core::cmp::Ordering::Equal => 0,
                core::cmp::Ordering::Greater => 1,
            })))
        }
        Function::MathTrunc => Ok(Value::Number(truncated(number_at(
            function, arguments, 0, span,
        )?))),
        // `NONE` at zero and below, and `NONE` for a result no float holds, on
        // `math::sqrt`'s reading: a NaN or an infinity compares false against
        // everything including itself, so it travels through a filter and an
        // ordering without ever saying it is not a number.
        Function::MathLn | Function::MathExp => {
            let number = number_at(function, arguments, 0, span)?;
            let Some(held) = number.as_float() else {
                return Err(Error::CallFailed {
                    function,
                    reason: "that number is outside the range a float holds",
                    span,
                });
            };
            let answer = if function == Function::MathLn {
                if held <= 0.0_f64 {
                    return Ok(Value::None);
                }
                held.ln()
            } else {
                held.exp()
            };
            if answer.is_finite() {
                Ok(Value::Number(Number::float(answer)))
            } else {
                Ok(Value::None)
            }
        }
        Function::ArrayConcat => {
            let first = array_at(function, arguments, 0, span)?.to_vec();
            let second = array_at(function, arguments, 1, span)?;
            let mut held = first;
            held.extend_from_slice(second);
            Ok(Value::Array(held))
        }
        // One more element, whatever kind it is. Appending an array as a value
        // and joining two arrays are different intentions, and a single
        // function deciding between them by the argument's kind is how a caller
        // appending a genuine array of two ends up with two elements.
        Function::ArrayAppend => {
            let mut held = array_at(function, arguments, 0, span)?.to_vec();
            held.push(arguments.get(1).cloned().unwrap_or(Value::None));
            Ok(Value::Array(held))
        }
        Function::ArrayIndexOf => {
            let items = array_at(function, arguments, 0, span)?;
            let wanted = arguments.get(1).cloned().unwrap_or(Value::None);
            match items.iter().position(|held| *held == wanted) {
                Some(at) => count(at, function, span),
                None => Ok(Value::None),
            }
        }
        // Folded by the **same accumulator the aggregates run**, rather than by
        // a second implementation here. These fold one array inside one record
        // and the aggregates fold a column across records — different
        // questions, and two answers to "which of these is smaller" or "what do
        // these add up to" would eventually differ on a decimal, a mixed group
        // or an empty one. One code path is how they cannot.
        Function::ArrayMin => folded(Aggregate::Min, function, arguments, span),
        Function::ArrayMax => folded(Aggregate::Max, function, arguments, span),
        Function::ArraySum => folded(Aggregate::Sum, function, arguments, span),
        Function::ObjectEntries => Ok(Value::Array(
            object_at(function, arguments, 0, span)?
                .iter()
                .map(|(name, value)| Value::Array(vec![Value::from(name.as_str()), value.clone()]))
                .collect(),
        )),
        // Whether the field is **there**, which is not the same question as
        // whether it holds something: a field explicitly holding `none` is
        // present, and this is the only way to tell the two apart.
        Function::ObjectHas => {
            let fields = object_at(function, arguments, 0, span)?;
            let name = text_at(function, arguments, 1, span)?;
            Ok(Value::Bool(fields.contains_key(name)))
        }
        Function::ObjectMerge => {
            let mut held = object_at(function, arguments, 0, span)?.clone();
            for (name, value) in object_at(function, arguments, 1, span)? {
                held.insert(name.clone(), value.clone());
            }
            Ok(Value::Object(held))
        }
        Function::StringConcat => {
            let first = text_at(function, arguments, 0, span)?;
            let second = text_at(function, arguments, 1, span)?;
            Ok(Value::from(format!("{first}{second}").as_str()))
        }
        Function::ArrayLen => {
            let items = array_at(function, arguments, 0, span)?;
            count(items.len(), function, span)
        }
        Function::ArrayFirst => Ok(array_at(function, arguments, 0, span)?
            .first()
            .cloned()
            .unwrap_or(Value::None)),
        Function::ArrayLast => Ok(array_at(function, arguments, 0, span)?
            .last()
            .cloned()
            .unwrap_or(Value::None)),
        Function::ObjectKeys => Ok(crate::collection::keys(object_at(
            function, arguments, 0, span,
        )?)),
        Function::ObjectValues => Ok(crate::collection::values(object_at(
            function, arguments, 0, span,
        )?)),
        Function::ObjectLen => {
            let object = object_at(function, arguments, 0, span)?;
            count(object.len(), function, span)
        }
        Function::ArrayDistinct => Ok(crate::collection::distinct(array_at(
            function, arguments, 0, span,
        )?)),
        Function::ArraySort => Ok(crate::collection::sort(array_at(
            function, arguments, 0, span,
        )?)),
        Function::ArrayReverse => Ok(crate::collection::reverse(array_at(
            function, arguments, 0, span,
        )?)),
        Function::ArrayFlatten => Ok(crate::collection::flatten(array_at(
            function, arguments, 0, span,
        )?)),
        Function::ArrayJoin => {
            let items = array_at(function, arguments, 0, span)?;
            let separator = text_at(function, arguments, 1, span)?;
            crate::collection::join(items, separator, span)
        }
        Function::ArraySlice => {
            let items = array_at(function, arguments, 0, span)?;
            let start = whole(function, arguments, 1, span)?;
            let count = whole(function, arguments, 2, span)?;
            crate::collection::slice(items, start, count, span)
        }
        Function::StringSplit => {
            let text = text_at(function, arguments, 0, span)?;
            let separator = text_at(function, arguments, 1, span)?;
            crate::text::split(text, separator, span)
        }
        Function::StringSlice => {
            let text = text_at(function, arguments, 0, span)?;
            let start = whole(function, arguments, 1, span)?;
            let count = whole(function, arguments, 2, span)?;
            crate::text::slice(text, start, count, span)
        }
        Function::StringLines => {
            let text = text_at(function, arguments, 0, span)?;
            let start = whole(function, arguments, 1, span)?;
            let count = whole(function, arguments, 2, span)?;
            crate::text::lines(text, start, count, span)
        }
        Function::StringReplace => {
            let text = text_at(function, arguments, 0, span)?;
            let from = text_at(function, arguments, 1, span)?;
            let to = text_at(function, arguments, 2, span)?;
            crate::text::replace(text, from, to, span)
        }
        // A root is a float whatever it was given, because most roots are not
        // exact in any of the three numeric kinds — so keeping the argument's
        // kind, which `math::abs` and its neighbours do, would mean rounding
        // `math::sqrt(2)` to `1` and calling it an integer.
        Function::MathSqrt => {
            let number = number_at(function, arguments, 0, span)?;
            let Some(held) = number.as_float() else {
                return Err(Error::CallFailed {
                    function,
                    reason: "that number is outside the range a float holds",
                    span,
                });
            };
            // A negative root is refused rather than answered with a NaN. A NaN
            // compares false against everything including itself, so it would
            // travel through a filter and an ordering silently; `math::abs`
            // says what a caller who meant the magnitude should write.
            if held < 0.0 {
                return Err(Error::CallFailed {
                    function,
                    reason: "a square root of a negative number is not a number; math::abs says the magnitude",
                    span,
                });
            }
            Ok(Value::Number(Number::float(held.sqrt())))
        }
        Function::MathPow => power(function, arguments, span),
        Function::MathAbs | Function::MathFloor | Function::MathCeil | Function::MathRound => {
            let number = number_at(function, arguments, 0, span)?;
            Ok(Value::Number(reshape(function, number)))
        }
        // Evaluated in the session, so the instant that reaches the log is a
        // value like any other — a replica applies what was written rather than
        // asking its own clock and reaching a different answer.
        Function::TimeNow => Ok(Value::Datetime(instant(span)?)),
        // The six readings of a date, each named separately rather than sharing
        // one arm with a match inside it. A shared arm needs a fallback for the
        // function the outer match already excluded, and a fallback here is an
        // invented answer: `time::month` returning a year is not a failure
        // anything downstream could detect.
        Function::TimeYear => reading(function, arguments, span, |civil| civil.year),
        Function::TimeMonth => reading(function, arguments, span, |civil| civil.month),
        Function::TimeDay => reading(function, arguments, span, |civil| civil.day),
        Function::TimeHour => reading(function, arguments, span, |civil| civil.hour),
        Function::TimeMinute => reading(function, arguments, span, |civil| civil.minute),
        Function::TimeSecond => reading(function, arguments, span, |civil| civil.second),
        // Whole seconds, and the sub-second remainder is **not** in the answer.
        // That is a loss taken deliberately, for the reason `type::float` takes
        // the nearest float for a decimal: strictness is affordable only where
        // the language already holds the sentence the caller should write
        // instead, and there is no `time::unix_millis` to write. `time::now()`
        // carries a remainder almost always, so refusing one would fail the
        // pairing everybody writes.
        Function::TimeUnix => Ok(Value::Number(Number::Integer(
            datetime_at(function, arguments, 0, span)?.seconds(),
        ))),
        Function::TimeFromUnix => from_unix(function, arguments, span),
        // The one call in this match that must not be evaluated above the
        // records. Nothing here enforces that — `plan::fold` does, by asking
        // `Function::purity` — and the arrangement is deliberate: an evaluator
        // that answered per record while the planner folded the call would still
        // hand out one identifier, so the guarantee belongs where the decision
        // to evaluate once is taken.
        Function::RandUuid => crate::generate::uuid(span),
        // A score needs the record's analyzer and the collection it is measured
        // against, and neither is a value — so it is answered in the evaluator,
        // where the scope is, and never reaches here.
        Function::SearchScore => Ok(Value::None),
        // A highlight needs the field's analyzer and what the read asked of that
        // field, for the same reason and by the same route.
        Function::SearchHighlight => Ok(Value::None),
        // The one function that makes a window sayable, and the reason
        // `GROUP BY` takes an expression: without it a caller would have to
        // store the bucket alongside the instant and keep the two in step.
        Function::TimeBucket => {
            let instant = datetime_at(function, arguments, 0, span)?;
            let width = duration_at(function, arguments, 1, span)?;
            bucket(function, instant, width, span)
        }
        Function::VectorCosine | Function::VectorEuclidean | Function::VectorDot => {
            let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
                return Err(wrong_type(function, 0, "a vector", "nothing", span));
            };
            Ok(crate::vector::distance(function, left, right))
        }
        // The predicate arrives as a function, so the seven arms differ in one
        // word each and there is no place for two of them to disagree about how
        // an argument is read.
        Function::GeoIntersects => {
            crate::geo::relate(function, tessari_geo::intersects, arguments, span)
        }
        Function::GeoDisjoint => {
            crate::geo::relate(function, tessari_geo::disjoint, arguments, span)
        }
        Function::GeoCovers => crate::geo::relate(function, tessari_geo::covers, arguments, span),
        Function::GeoCoveredBy => {
            crate::geo::relate(function, tessari_geo::covered_by, arguments, span)
        }
        Function::GeoContains => {
            crate::geo::relate(function, tessari_geo::contains, arguments, span)
        }
        Function::GeoWithin => crate::geo::relate(function, tessari_geo::within, arguments, span),
        Function::GeoEquals => crate::geo::relate(function, tessari_geo::equals, arguments, span),
        Function::GeoTouches => crate::geo::relate(function, tessari_geo::touches, arguments, span),
        Function::GeoDistance => crate::geo::separation(function, arguments, span),
        Function::GeoArea => crate::geo::ground(function, arguments, span),
        Function::TypeOf => {
            let Some(value) = arguments.first() else {
                return Err(wrong_type(function, 0, "a value", "nothing", span));
            };
            Ok(Value::from(value.type_name()))
        }
        // A cast either produces the kind it names or refuses; `crate::cast`
        // holds the reading for each, and the reason it refuses rather than
        // answering `none`.
        Function::TypeBool
        | Function::TypeInt
        | Function::TypeFloat
        | Function::TypeString
        | Function::TypeDatetime
        | Function::TypeUuid => {
            let Some(value) = arguments.first() else {
                return Err(wrong_type(function, 0, "a value", "nothing", span));
            };
            crate::cast::read(function, value, span)
        }
    }
}

/// The instant this statement is being evaluated at.
///
/// Read once, in the session, so the value that reaches the log is a value like
/// any other — a replica applies what was written rather than asking its own
/// clock and reaching a different answer. A clock before the epoch is refused
/// rather than folded to zero: a machine whose time is wrong should say so.
/// The start of the window `instant` falls in.
///
/// Windows are anchored at the **epoch**, not at the first record, so the same
/// instant lands in the same window in every query, every process and every
/// replica. A window anchored at whatever data happened to arrive first would
/// give two callers different answers to the same question, and neither would
/// notice.
///
/// Truncation is toward negative infinity — `div_euclid` and not division — so
/// an instant before the epoch lands in the window that *contains* it rather
/// than the one after it. That is the difference between a window boundary and
/// an off-by-one nobody sees until they query a date in 1969.
fn bucket(
    function: Function,
    instant: &tessari_types::Datetime,
    width: &tessari_types::Duration,
    span: Span,
) -> Result<Value> {
    let seconds = width.seconds();
    if seconds <= 0 && width.nanos() == 0 {
        return Err(Error::CallFailed {
            function,
            reason: "a window has to be longer than nothing",
            span,
        });
    }
    // Whole seconds only: a sub-second window is a real thing and needs the
    // nanosecond remainder in the arithmetic, which is a different function from
    // the one anybody asks for. Refused rather than rounded, because rounding
    // here would silently answer a question nobody asked.
    if width.nanos() != 0 {
        return Err(Error::CallFailed {
            function,
            reason: "a window is a whole number of seconds",
            span,
        });
    }
    let start = instant
        .seconds()
        .div_euclid(seconds)
        .saturating_mul(seconds);
    tessari_types::Datetime::new(start, 0).map_or_else(
        || {
            Err(Error::CallFailed {
                function,
                reason: "that window does not start at an instant this type holds",
                span,
            })
        },
        |held| Ok(Value::Datetime(held)),
    )
}

/// One field of the date an instant falls on.
///
/// The date is read by [`Datetime::civil`], which is the store's single answer
/// to how a second count becomes a calendar date. Deriving it here as well would
/// be a second copy of the era arithmetic, and the way two copies fail is that
/// they disagree on one day in four hundred years while both keep answering.
fn reading(
    function: Function,
    arguments: &[Value],
    span: Span,
    take: impl Fn(tessari_types::Civil) -> i64,
) -> Result<Value> {
    let instant = datetime_at(function, arguments, 0, span)?;
    Ok(Value::Number(Number::Integer(take(instant.civil()))))
}

/// The instant a second count names.
///
/// A fraction is refused rather than truncated, on the rule the casts follow:
/// `math::round` already says which whole number was meant, so choosing one here
/// would answer a question the caller did not ask. A count no instant holds is
/// refused for the same reason it is never clamped — a clamped instant is a
/// moment the author did not write, sitting in a record that will be read back.
fn from_unix(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let Some(seconds) = number.as_exact_integer() else {
        return Err(Error::CallFailed {
            function,
            reason: "a unix time is a whole number of seconds an instant holds; \
                     math::round says which whole one was meant",
            span,
        });
    };
    Ok(Value::Datetime(Datetime::from_seconds(seconds)))
}

/// The instant this process's clock reads.
///
/// Shared with the queue engine, which needs the same instant for the same
/// reason `time::now()` gives it to a statement: a claim's deadline is computed
/// once here and **written**, so the value that reaches the log is one every
/// node agrees about rather than a computation each of them repeats against its
/// own clock.
///
/// It reports through [`Function::TimeNow`] whoever asks, because there is one
/// clock and a caller reading a failure wants to know which one could not be
/// read — not which internal path asked it.
pub(crate) fn instant(span: Span) -> Result<Datetime> {
    let failed = |reason: &'static str| Error::CallFailed {
        function: Function::TimeNow,
        reason,
        span,
    };
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| failed("the clock is before the epoch"))?;
    let seconds = i64::try_from(since.as_secs()).map_err(|_| failed("the clock is unreadable"))?;
    Datetime::new(seconds, since.subsec_nanos()).ok_or_else(|| failed("the clock is unreadable"))
}

/// An element count as a value, refusing one no integer can hold.
fn count(size: usize, function: Function, span: Span) -> Result<Value> {
    let Ok(size) = i64::try_from(size) else {
        return Err(Error::CallFailed {
            function,
            reason: "the count is outside the integer range",
            span,
        });
    };
    Ok(Value::Number(Number::Integer(size)))
}

/// The four shapes `math::*` gives a number, each keeping its kind.
fn reshape(function: Function, number: &Number) -> Number {
    match (function, number) {
        (Function::MathAbs, Number::Integer(held)) => Number::Integer(held.saturating_abs()),
        (Function::MathAbs, Number::Decimal(held)) => Number::Decimal(held.abs()),
        (Function::MathAbs, Number::Float(held)) => Number::float(held.abs()),
        (Function::MathFloor, Number::Decimal(held)) => Number::Decimal(held.floor()),
        (Function::MathFloor, Number::Float(held)) => Number::float(held.floor()),
        (Function::MathCeil, Number::Decimal(held)) => Number::Decimal(held.ceil()),
        (Function::MathCeil, Number::Float(held)) => Number::float(held.ceil()),
        (Function::MathRound, Number::Decimal(held)) => Number::Decimal(
            held.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero),
        ),
        (Function::MathRound, Number::Float(held)) => Number::float(held.round()),
        // An integer is already whole, so flooring, ceiling and rounding one is
        // the number itself rather than a conversion nobody asked for.
        (_, held) => held.clone(),
    }
}

fn text_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&str> {
    match arguments.get(at) {
        Some(Value::String(text)) => Ok(text),
        other => Err(wrong_type(function, at, "a string", named(other), span)),
    }
}

/// One array folded by the accumulator the aggregates use.
///
/// The array is the group. Every value in it is offered in order, exactly as a
/// record's value is offered when the fold is over rows, so the promotion
/// rules, the treatment of an absence and the answer over nothing are not
/// restated here — they are the ones already written down and already tested.
fn folded(
    aggregate: Aggregate,
    function: Function,
    arguments: &[Value],
    span: Span,
) -> Result<Value> {
    let items = array_at(function, arguments, 0, span)?;
    let mut running = crate::accumulate::Accumulator::for_aggregate(aggregate, span);
    for value in items {
        running.offer(value)?;
    }
    running.finish()
}

fn bytes_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&[u8]> {
    match arguments.get(at) {
        Some(Value::Bytes(held)) => Ok(held),
        other => Err(wrong_type(function, at, "bytes", named(other), span)),
    }
}

/// A number's whole part, toward zero, keeping its kind.
///
/// Its own function rather than an arm of [`reshape`]: that one is the four
/// shapes `math::abs` and its neighbours give a number and its match is written
/// per kind, and adding a fifth there would grow a table whose whole point is
/// to be read at a glance.
fn truncated(number: &Number) -> Number {
    match number {
        Number::Integer(held) => Number::Integer(*held),
        Number::Decimal(held) => Number::Decimal(held.trunc()),
        Number::Float(held) => Number::float(held.trunc()),
    }
}

fn array_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&[Value]> {
    match arguments.get(at) {
        Some(Value::Array(items)) => Ok(items),
        other => Err(wrong_type(function, at, "an array", named(other), span)),
    }
}

/// A base raised to an exponent.
///
/// **Two whole numbers answer a whole number**, which is the rule `math::abs`
/// and its neighbours already follow — a kind is kept where keeping it is
/// exact. `math::pow(2, 10)` is therefore `1024` and not `1024.0`. Anything
/// else — a fractional base, a fractional or negative exponent — answers a
/// float, since that is the only kind that holds the answer.
///
/// An integer result too large to hold is **refused rather than saturated**. A
/// saturated power is a wrong number that looks like a right one, and it would
/// be the largest number in the store, which is exactly the value most likely
/// to pass a sanity check unnoticed.
fn power(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let base = number_at(function, arguments, 0, span)?;
    let exponent = number_at(function, arguments, 1, span)?;
    let failed = |reason: &'static str| Error::CallFailed {
        function,
        reason,
        span,
    };
    if let (Some(base), Some(exponent)) = (base.as_exact_integer(), exponent.as_exact_integer())
        && exponent >= 0
    {
        let Ok(exponent) = u32::try_from(exponent) else {
            return Err(failed("that exponent is larger than any integer answer"));
        };
        return base
            .checked_pow(exponent)
            .map(|held| Value::Number(Number::Integer(held)))
            .ok_or_else(|| failed("that power is outside the integer range"));
    }
    let (Some(base), Some(exponent)) = (base.as_float(), exponent.as_float()) else {
        return Err(failed("that number is outside the range a float holds"));
    };
    let held = base.powf(exponent);
    if held.is_nan() || held.is_infinite() {
        return Err(failed("that power is not a number a float holds"));
    }
    Ok(Value::Number(Number::float(held)))
}

fn object_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&std::collections::BTreeMap<String, Value>> {
    match arguments.get(at) {
        Some(Value::Object(fields)) => Ok(fields),
        other => Err(wrong_type(function, at, "an object", named(other), span)),
    }
}

/// An argument that has to be a whole number, for a position or a count.
///
/// A fraction is refused rather than truncated, on the rule the casts follow:
/// `math::round` already says which whole number was meant.
fn whole(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<i64> {
    let number = number_at(function, arguments, at, span)?;
    number.as_exact_integer().ok_or(Error::CallFailed {
        function,
        reason: "a position is a whole number; math::round says which one was meant",
        span,
    })
}

fn datetime_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&tessari_types::Datetime> {
    match arguments.get(at) {
        Some(Value::Datetime(held)) => Ok(held),
        other => Err(wrong_type(function, at, "a datetime", named(other), span)),
    }
}

fn duration_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&tessari_types::Duration> {
    match arguments.get(at) {
        Some(Value::Duration(held)) => Ok(held),
        other => Err(wrong_type(function, at, "a duration", named(other), span)),
    }
}

fn number_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&Number> {
    match arguments.get(at) {
        Some(Value::Number(number)) => Ok(number),
        other => Err(wrong_type(function, at, "a number", named(other), span)),
    }
}

fn named(value: Option<&Value>) -> &'static str {
    value.map_or("nothing", Value::type_name)
}

fn wrong_type(
    function: Function,
    at: usize,
    expected: &'static str,
    found: &'static str,
    span: Span,
) -> Error {
    Error::WrongArgument {
        function,
        at: at.saturating_add(1),
        expected,
        found,
        span,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_ql::{Function, Span};
    use tessari_types::{Number, Value};

    use super::call;

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn text(value: &str) -> Value {
        Value::from(value)
    }

    #[test]
    fn a_length_counts_characters_and_not_bytes() {
        let answer = call(Function::StringLen, &[text("héllo")], at()).expect("a length");
        assert_eq!(answer, Value::Number(Number::Integer(5)));
    }

    #[test]
    fn the_last_element_is_the_one_a_path_cannot_reach() {
        let items = Value::Array(vec![text("a"), text("b"), text("c")]);
        assert_eq!(
            call(Function::ArrayLast, &[items], at()).expect("an element"),
            text("c")
        );
    }

    #[test]
    fn an_empty_array_answers_with_an_absence_rather_than_a_failure() {
        // An array having no first element is a state, not a mistake — the same
        // answer a route reaching nothing gives.
        let empty = Value::Array(Vec::new());
        assert_eq!(
            call(Function::ArrayFirst, &[empty], at()).expect("an answer"),
            Value::None
        );
    }

    #[test]
    fn an_absent_argument_answers_with_an_absence_rather_than_failing() {
        // Without this a single record missing a field would fail a whole read,
        // and a store holding documents of differing shapes cannot have a
        // function surface that only works when every record has every field.
        assert_eq!(
            call(Function::StringLen, &[Value::None], at()).expect("an answer"),
            Value::None
        );
        assert_eq!(
            call(Function::ArrayLen, &[Value::Null], at()).expect("an answer"),
            Value::None
        );
        // Except for the one function that asks about the value itself.
        assert_eq!(
            call(Function::TypeOf, &[Value::None], at()).expect("an answer"),
            text("none")
        );
    }

    #[test]
    fn a_wrong_argument_names_the_function_the_position_and_both_types() {
        let error = call(
            Function::StringLen,
            &[Value::Number(Number::Integer(1))],
            at(),
        )
        .expect_err("a refusal");
        let message = error.to_string();
        assert!(message.contains("string::len"), "{message}");
        assert!(message.contains("argument 1"), "{message}");
        assert!(message.contains("string"), "{message}");
        assert!(message.contains("number"), "{message}");
    }

    #[test]
    fn rounding_keeps_the_kind_it_was_given() {
        let whole = Value::Number(Number::Integer(7));
        assert_eq!(
            call(Function::MathFloor, std::slice::from_ref(&whole), at()).expect("a number"),
            whole
        );
        let fractional = Value::Number(Number::float(2.5));
        assert_eq!(
            call(Function::MathRound, &[fractional], at()).expect("a number"),
            Value::Number(Number::float(3.0))
        );
    }

    /// The instant every `time::` test below reads.
    fn instant(text: &str) -> Value {
        Value::Datetime(tessari_types::Datetime::parse_rfc3339(text).expect("an instant"))
    }

    fn read(function: Function, value: Value) -> Value {
        call(function, &[value], at()).expect("a reading")
    }

    #[test]
    fn each_reading_takes_its_own_field_and_not_a_neighbours() {
        // The failure this guards is a copy-paste one: six arms differing by a
        // single field name, where `time::minute` returning the hour is not a
        // crash and not a type error. The fixture has six distinct values so no
        // two fields can be swapped without the test noticing.
        let taken = instant("2026-08-28T14:37:09Z");
        for (function, expected) in [
            (Function::TimeYear, 2026),
            (Function::TimeMonth, 8),
            (Function::TimeDay, 28),
            (Function::TimeHour, 14),
            (Function::TimeMinute, 37),
            (Function::TimeSecond, 9),
        ] {
            assert_eq!(
                read(function, taken.clone()),
                Value::Number(Number::Integer(expected)),
                "{function}"
            );
        }
    }

    #[test]
    fn the_second_of_the_minute_is_not_the_second_since_the_epoch() {
        // Two functions one letter apart in meaning, and both answer an integer,
        // so nothing but this asserts which is which.
        let taken = instant("2026-08-28T14:37:09Z");
        assert_eq!(
            read(Function::TimeSecond, taken.clone()),
            Value::Number(Number::Integer(9))
        );
        assert_eq!(
            read(Function::TimeUnix, taken),
            Value::Number(Number::Integer(1_787_927_829))
        );
    }

    #[test]
    fn an_instant_survives_a_round_trip_through_its_second_count() {
        let taken = instant("2026-08-28T14:37:09Z");
        let seconds = read(Function::TimeUnix, taken.clone());
        assert_eq!(read(Function::TimeFromUnix, seconds), taken);
    }

    #[test]
    fn a_sub_second_remainder_is_dropped_by_the_second_count_and_not_refused() {
        // Deliberate, and the reason is in `time::unix`'s comment: there is no
        // `time::unix_millis` to write instead, and `time::now()` carries a
        // remainder nearly always. Asserted so the loss is a decision on the
        // record rather than something nobody looked at.
        let taken = instant("2026-08-28T14:37:09.5Z");
        assert_eq!(
            read(Function::TimeUnix, taken),
            Value::Number(Number::Integer(1_787_927_829))
        );
    }

    #[test]
    fn a_fractional_second_count_is_refused_rather_than_truncated() {
        let error = call(
            Function::TimeFromUnix,
            &[Value::Number(Number::float(1.5))],
            at(),
        )
        .expect_err("a refusal");
        let message = error.to_string();
        assert!(message.contains("time::from_unix"), "{message}");
        assert!(message.contains("math::round"), "{message}");
        // A whole number written as a float is not a fraction, and is taken.
        assert_eq!(
            read(Function::TimeFromUnix, Value::Number(Number::float(0.0))),
            Value::Datetime(tessari_types::Datetime::from_seconds(0))
        );
    }

    #[test]
    fn a_reading_of_an_absent_instant_is_an_absence_and_narrows_the_read() {
        // None of the eight join `answers_for_absence`: there is no year that a
        // missing field has. The general rule therefore applies, and a record
        // without the field drops out of the read instead of failing it.
        assert_eq!(
            call(Function::TimeYear, &[Value::None], at()).expect("an answer"),
            Value::None
        );
    }
}
