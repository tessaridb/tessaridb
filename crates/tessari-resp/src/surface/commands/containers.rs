//! What the four container kinds look like once read, shared by the hash,
//! list, set and sorted-set commands (G079 C6).
//!
//! A write is one engine statement (`HASH SET`, `LIST PUSH`, …) that reads the
//! key and writes it, so no update is lost; a read is one `GET` of the whole
//! value, and the command's answer is computed here from it. The kinds are
//! told apart the way the engine tells them: an array is a list, an object a
//! hash, a set of `[score, member]` pairs a sorted set and any other set a set.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher};

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::replies;
use crate::surface::statement::{Binder, value_of};
use crate::surface::values;
use tessaridb::Outcome;

/// The kind a held value is, as `TYPE` names it.
pub(crate) fn kind(value: &Value) -> &'static str {
    match value {
        Value::None => "none",
        Value::Array(_) => "list",
        Value::Object(_) => "hash",
        Value::Set(members) if ranking(members).is_some() => "zset",
        Value::Set(_) => "set",
        _ => "string",
    }
}

/// The one value the first outcome holds.
pub(super) fn first(outcomes: Vec<Outcome>) -> Value {
    outcomes.into_iter().next().map_or(Value::None, value_of)
}

/// A held hash, empty for no key, or `WRONGTYPE`.
pub(super) fn hash(value: Value) -> Result<BTreeMap<String, Value>, Reply> {
    match value {
        Value::None => Ok(BTreeMap::new()),
        Value::Object(fields) => Ok(fields),
        _ => Err(replies::wrong_type()),
    }
}

/// A held list, empty for no key, or `WRONGTYPE`.
pub(super) fn list(value: Value) -> Result<Vec<Value>, Reply> {
    match value {
        Value::None => Ok(Vec::new()),
        Value::Array(items) => Ok(items),
        _ => Err(replies::wrong_type()),
    }
}

/// A held set of members, empty for no key, or `WRONGTYPE`.
pub(super) fn members(value: Value) -> Result<BTreeSet<Value>, Reply> {
    match value {
        Value::None => Ok(BTreeSet::new()),
        Value::Set(members) if ranking(&members).is_none() => Ok(members),
        _ => Err(replies::wrong_type()),
    }
}

/// A held sorted set as `(member, score)`, lowest first, empty for no key, or
/// `WRONGTYPE`.
pub(super) fn ranked(value: Value) -> Result<Vec<(String, f64)>, Reply> {
    match value {
        Value::None => Ok(Vec::new()),
        Value::Set(pairs) => ranking(&pairs).ok_or_else(replies::wrong_type),
        _ => Err(replies::wrong_type()),
    }
}

/// A ranking as the store holds one: a set of `[score, member]` pairs.
pub(super) fn held_ranking(pairs: &[(String, f64)]) -> Value {
    Value::Set(
        pairs
            .iter()
            .map(|(member, at)| {
                Value::Array(vec![
                    Value::Number(Number::Float(*at)),
                    Value::String(member.clone()),
                ])
            })
            .collect(),
    )
}

/// A sorted-set source, which may also be a plain set: each member then
/// scores 1, as the reference treats a set handed to `ZUNION` and its kin.
pub(super) fn ranked_or_members(value: Value) -> Result<Vec<(String, f64)>, Reply> {
    if let Value::Set(held) = &value
        && ranking(held).is_none()
    {
        return held
            .iter()
            .map(|member| {
                values::rendered(member)
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|member| (member, 1.0))
                    .ok_or_else(replies::wrong_type)
            })
            .collect();
    }
    ranked(value)
}

/// The pairs of a set that is a ranking, in its order; `None` when it is not.
fn ranking(pairs: &BTreeSet<Value>) -> Option<Vec<(String, f64)>> {
    if pairs.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| match pair {
            Value::Array(pair) => match pair.as_slice() {
                [Value::Number(Number::Float(score)), Value::String(member)] => {
                    Some((member.clone(), *score))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// An element's reply: its bytes.
pub(super) fn bulk(value: &Value) -> Reply {
    values::rendered(value).map_or(Reply::Nil, Reply::Bulk)
}

/// A score's reply, as the shortest digits that read back as it — a double
/// to a RESP3 client.
pub(super) fn score(score: f64) -> Reply {
    Reply::Double(values::float_text(score))
}

/// A field or member name, which the store holds as text.
pub(super) fn text(bytes: &[u8]) -> Result<String, Reply> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Reply::error("ERR a field or member name on this server is UTF-8 text"))
}

/// `$name = $value, …` for each pair, both bound — the name as text.
pub(super) fn pairs(
    pairs: &[Vec<u8>],
    binder: &mut Binder,
    value: impl Fn(&[u8]) -> Result<Value, Reply>,
) -> Result<String, Reply> {
    let mut written = Vec::with_capacity(pairs.len() / 2);
    for pair in pairs.chunks(2) {
        let [name, bytes] = pair else {
            return Err(replies::syntax());
        };
        let name = binder.bind(Value::String(text(name)?));
        let bound = binder.bind(value(bytes)?);
        written.push(format!("{name} = {bound}"));
    }
    Ok(written.join(", "))
}

/// Each value bound as it is stored, comma-separated.
pub(super) fn stored(items: &[Vec<u8>], binder: &mut Binder) -> String {
    items
        .iter()
        .map(|item| binder.bind(values::stored(item)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A count as the integer reply.
pub(super) fn integer(count: usize) -> Reply {
    Reply::Integer(i64::try_from(count).unwrap_or(i64::MAX))
}

/// The slice from `start` to `stop`, both included, each counted from the back
/// when negative and clamped to `length` — the bounds `LRANGE` and `ZRANGE`
/// take.
pub(super) fn span(length: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    let length = i64::try_from(length).ok()?;
    let at = |index: i64| {
        if index < 0 {
            length.saturating_add(index)
        } else {
            index
        }
    };
    let (start, stop) = (at(start).max(0), at(stop).min(length.saturating_sub(1)));
    if start > stop || length == 0 {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(stop).ok()?))
}

/// An index into `0..length` chosen at random — for the commands that answer
/// a random element, where the protocol promises no more than that.
pub(super) fn random(length: usize) -> usize {
    if length == 0 {
        return 0;
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_usize(length);
    let drawn = usize::try_from(hasher.finish()).unwrap_or(usize::MAX);
    drawn.checked_rem(length).unwrap_or(0)
}

/// The most elements a repeating random read answers, whatever count it asks.
pub(super) const RANDOM_CEILING: u64 = 1_000_000;
