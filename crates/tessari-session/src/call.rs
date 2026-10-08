//! What each of the language's functions does, once its arguments are values.
//!
//! Arity was checked when the statement was read (`tessari-ql`'s `Function`
//! knows the set), so what is left here is what each argument *holds* — which
//! nothing could know before a record was in hand.
//!
//! A wrong type names the function, the position, what was wanted and what was
//! there. A message saying only "wrong type" makes the author guess which of
//! three arguments it meant.

mod arguments;
mod numbers;
mod time;
use tessari_ql::{Aggregate, Function, Span};
use tessari_types::{Number, Value};

use crate::error::Result;
pub(crate) use arguments::{
    array_at, bytes_at, datetime_at, duration_at, number_at, object_at, text_at, whole, wrong_type,
};
pub(crate) use numbers::{count, folded, power, reshape, truncated};
pub(crate) use time::{bucket, from_secs, from_unix, instant, reading};

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
        // Text that is not one JSON value answers `NONE`, on the decoders'
        // reading above: the kind was checked, and what is left is a value.
        Function::JsonParse => Ok(tessari_types::json::read(
            text_at(function, arguments, 0, span)?.as_bytes(),
        )
        .unwrap_or(Value::None)),
        // Reached with no catalog in hand — a statement's own evaluation names
        // the tables first (`Session::json_encode`) — so a reference is spelled
        // by its table's id, visibly, as the HTTP surface spells a dropped one.
        Function::JsonEncode => {
            let mut out = String::new();
            tessari_types::json::write(
                &mut out,
                arguments.first().unwrap_or(&Value::None),
                &tessari_types::json::Names::new(),
            );
            Ok(Value::from(out.as_str()))
        }
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
        Function::MathMin | Function::MathMax => numbers::extreme(function, arguments, span),
        Function::MathSign => numbers::sign(function, arguments, span),
        Function::MathTrunc => Ok(Value::Number(truncated(number_at(
            function, arguments, 0, span,
        )?))),
        // `NONE` at zero and below, and `NONE` for a result no float holds, on
        // `math::sqrt`'s reading: a NaN or an infinity compares false against
        // everything including itself, so it travels through a filter and an
        // ordering without ever saying it is not a number.
        Function::MathLn | Function::MathExp => numbers::logarithm(function, arguments, span),
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
        Function::MathSqrt => numbers::square_root(function, arguments, span),
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
        Function::DurationFromSecs => from_secs(function, arguments, span),
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
        Function::SearchScore | Function::SearchExplain => Ok(Value::None),
        // A highlight needs the field's analyzer and what the read asked of that
        // field, for the same reason and by the same route.
        Function::SearchHighlight => Ok(Value::None),
        // A rank is the fusion's, answered where the scope carries it.
        Function::SearchRanks => Ok(Value::None),
        // A search's own answers about its record, answered in the evaluator
        // where the record's hit is in scope (ADR-0105).
        Function::SearchTable | Function::SearchSnippet => Ok(Value::None),
        // The session's node and tenancy are not values either, and are answered
        // in the evaluator, where the session is.
        Function::SessionContext => Ok(Value::None),
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
        Function::GeoCell => crate::geo::cell(function, arguments, span),
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

#[cfg(test)]
mod tests;
