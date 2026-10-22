//! The hash commands, as `HASH` statements and reads of the whole hash.

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{self, bulk, first, hash, pairs, text};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};
use crate::surface::values;

const NAMES: &[&str] = &[
    "HSET",
    "HMSET",
    "HSETNX",
    "HGET",
    "HMGET",
    "HGETALL",
    "HDEL",
    "HEXISTS",
    "HLEN",
    "HKEYS",
    "HVALS",
    "HSTRLEN",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HRANDFIELD",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// A read of the whole hash, answered by `answer`.
fn read(
    target: String,
    answer: impl FnOnce(std::collections::BTreeMap<String, Value>) -> Reply + Send + 'static,
) -> Planned {
    Planned::one(
        format!("GET {target}"),
        Box::new(move |outcomes| match hash(first(outcomes)) {
            Ok(fields) => answer(fields),
            Err(reply) => reply,
        }),
    )
}

/// The bytes of a stored value as a client reads them.
fn stored(bytes: &[u8]) -> Result<Value, Reply> {
    Ok(values::stored(bytes))
}

/// Plan `name` when it is a hash command.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let [key, rest @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let target = binder.key(space, key);
    let planned = match name {
        "HSET" | "HMSET" => {
            if rest.is_empty() || rest.len() % 2 != 0 {
                return Err(replies::arity(name));
            }
            let written = pairs(rest, binder, stored)?;
            let ok = name == "HMSET";
            Planned::one(
                format!("HASH SET {target} {written}"),
                Box::new(move |outcomes| match first(outcomes) {
                    _ if ok => Reply::OK,
                    Value::Number(Number::Integer(added)) => Reply::Integer(added),
                    _ => Reply::Integer(0),
                }),
            )
        }
        "HSETNX" => {
            let [_, _] = rest else {
                return Err(replies::arity(name));
            };
            let written = pairs(rest, binder, stored)?;
            Planned::one(
                format!("HASH SET {target} {written} IF ABSENT"),
                Box::new(|outcomes| match first(outcomes) {
                    Value::Number(Number::Integer(added)) => Reply::Integer(added),
                    _ => Reply::Integer(0),
                }),
            )
        }
        "HDEL" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let fields = rest
                .iter()
                .map(|field| Ok(binder.bind(Value::String(text(field)?))))
                .collect::<Result<Vec<_>, Reply>>()?
                .join(", ");
            Planned::one(
                format!("HASH REMOVE {target} {fields}"),
                Box::new(|outcomes| match first(outcomes) {
                    Value::Number(Number::Integer(removed)) => Reply::Integer(removed),
                    _ => Reply::Integer(0),
                }),
            )
        }
        "HINCRBY" | "HINCRBYFLOAT" => {
            let [field, by] = rest else {
                return Err(replies::arity(name));
            };
            let floating = name == "HINCRBYFLOAT";
            let amount = if floating {
                Value::Number(Number::float(
                    values::float(by).ok_or_else(replies::not_a_float)?,
                ))
            } else {
                Value::Number(Number::Integer(integer(by)?))
            };
            let field = binder.bind(Value::String(text(field)?));
            let amount = binder.bind(amount);
            let refused = if floating {
                Reply::error("ERR hash value is not a float")
            } else {
                Reply::error("ERR hash value is not an integer")
            };
            Planned::one(
                format!("HASH INCR {target} {field} BY {amount}"),
                Box::new(move |outcomes| match first(outcomes) {
                    Value::Number(Number::Integer(next)) if !floating => Reply::Integer(next),
                    Value::Number(number) if floating => number
                        .as_float()
                        .map_or_else(replies::not_a_float, containers::score),
                    _ if floating => replies::not_a_float(),
                    _ => replies::not_an_integer(),
                }),
            )
            .refusing_invalid_as(refused)
        }
        _ => return reads(name, rest, target).map(Some),
    };
    Ok(Some(Step::Statements(planned)))
}

/// The commands that read the hash and change nothing.
fn reads(name: &str, rest: &[Vec<u8>], target: String) -> Result<Step, Reply> {
    let planned = match name {
        "HGET" | "HEXISTS" | "HSTRLEN" => {
            let [field] = rest else {
                return Err(replies::arity(name));
            };
            let field = text(field)?;
            let answer = name.to_owned();
            read(target, move |fields| {
                let held = fields.get(&field);
                match answer.as_str() {
                    "HEXISTS" => Reply::Integer(i64::from(held.is_some())),
                    "HSTRLEN" => containers::integer(
                        held.and_then(values::rendered)
                            .map_or(0, |bytes| bytes.len()),
                    ),
                    _ => held.map_or(Reply::Nil, bulk),
                }
            })
        }
        "HMGET" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let named = rest
                .iter()
                .map(|field| text(field))
                .collect::<Result<Vec<_>, _>>()?;
            read(target, move |fields| {
                Reply::Array(
                    named
                        .iter()
                        .map(|field| fields.get(field).map_or(Reply::Nil, bulk))
                        .collect(),
                )
            })
        }
        "HGETALL" | "HKEYS" | "HVALS" | "HLEN" => {
            if !rest.is_empty() {
                return Err(replies::arity(name));
            }
            let answer = name.to_owned();
            read(target, move |fields| match answer.as_str() {
                "HLEN" => containers::integer(fields.len()),
                "HKEYS" => Reply::Array(fields.into_keys().map(Reply::text).collect()),
                "HVALS" => Reply::Array(fields.values().map(bulk).collect()),
                _ => Reply::Map(
                    fields
                        .iter()
                        .map(|(field, value)| (Reply::text(field.clone()), bulk(value)))
                        .collect(),
                ),
            })
        }
        "HRANDFIELD" => {
            let (count, with_values) = match rest {
                [] => (None, false),
                [count] => (Some(integer(count)?), false),
                [count, with] if with.eq_ignore_ascii_case(b"WITHVALUES") => {
                    (Some(integer(count)?), true)
                }
                _ => return Err(replies::syntax()),
            };
            read(target, move |fields| {
                random_fields(&fields, count, with_values)
            })
        }
        _ => return Err(replies::unknown(name.as_bytes())),
    };
    Ok(Step::Statements(planned))
}

/// `HRANDFIELD`: one field, or `count` distinct ones, or with a negative count
/// that many that may repeat.
fn random_fields(
    fields: &std::collections::BTreeMap<String, Value>,
    count: Option<i64>,
    with_values: bool,
) -> Reply {
    let held: Vec<(&String, &Value)> = fields.iter().collect();
    let Some(count) = count else {
        return held
            .get(containers::random(held.len()))
            .map_or(Reply::Nil, |(field, _)| Reply::text((*field).clone()));
    };
    let chosen: Vec<(&String, &Value)> = if count >= 0 {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX).min(held.len());
        let mut left = held.clone();
        (0..wanted)
            .map(|_| left.swap_remove(containers::random(left.len())))
            .collect()
    } else if held.is_empty() {
        Vec::new()
    } else {
        (0..count.unsigned_abs().min(containers::RANDOM_CEILING))
            .filter_map(|_| held.get(containers::random(held.len())).copied())
            .collect()
    };
    if with_values {
        return Reply::Pairs(
            chosen
                .into_iter()
                .map(|(field, value)| (Reply::text(field.clone()), bulk(value)))
                .collect(),
        );
    }
    Reply::Array(
        chosen
            .into_iter()
            .map(|(field, _)| Reply::text(field.clone()))
            .collect(),
    )
}
