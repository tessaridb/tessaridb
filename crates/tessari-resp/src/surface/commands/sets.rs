//! The set commands, as `MEMBERS` statements and reads of whole sets.

use std::collections::BTreeSet;

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{self, bulk, first, members, stored};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned, values_of};
use crate::surface::values;

const NAMES: &[&str] = &[
    "SADD",
    "SREM",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SPOP",
    "SRANDMEMBER",
    "SMOVE",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

fn counted(outcomes: Vec<tessaridb::Outcome>) -> Reply {
    match first(outcomes) {
        Value::Number(Number::Integer(count)) => Reply::Integer(count),
        _ => Reply::Integer(0),
    }
}

/// A read of one whole set, answered by `answer`.
fn read(target: &str, answer: impl FnOnce(BTreeSet<Value>) -> Reply + Send + 'static) -> Planned {
    Planned::one(
        format!("GET {target}"),
        Box::new(move |outcomes| match members(first(outcomes)) {
            Ok(held) => answer(held),
            Err(reply) => reply,
        }),
    )
}

/// Plan `name` when it is a set command.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    if matches!(name, "SINTER" | "SUNION" | "SDIFF" | "SINTERCARD") {
        return combined(name, arguments, space, binder)
            .map(|planned| Some(Step::Statements(planned)));
    }
    let [key, rest @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let target = binder.key(space, key);
    let planned = match name {
        "SADD" | "SREM" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let verb = if name == "SADD" { "ADD" } else { "REMOVE" };
            let values = stored(rest, binder);
            Planned::one(
                format!("MEMBERS {verb} {target} {values}"),
                Box::new(counted),
            )
        }
        "SPOP" => match rest {
            [] => Planned::one(
                format!("MEMBERS POP {target}"),
                Box::new(|outcomes| bulk(&first(outcomes))),
            ),
            [count] => {
                let count = integer(count)?;
                if count < 0 {
                    return Err(Reply::error("ERR value is out of range, must be positive"));
                }
                let bound = binder.bind(Value::Number(Number::Integer(count)));
                Planned::one(
                    format!("MEMBERS POP {target} LIMIT {bound}"),
                    Box::new(|outcomes| match first(outcomes) {
                        Value::Array(items) => Reply::Array(items.iter().map(bulk).collect()),
                        _ => Reply::Array(Vec::new()),
                    }),
                )
            }
            _ => return Err(replies::arity(name)),
        },
        "SMOVE" => {
            let [to, member] = rest else {
                return Err(replies::arity(name));
            };
            let to = binder.key(space, to);
            let member = binder.bind(values::stored(member));
            Planned::one(
                format!("MEMBERS MOVE {target} {member} TO {to}"),
                Box::new(|outcomes| {
                    Reply::Integer(i64::from(first(outcomes) == Value::Bool(true)))
                }),
            )
        }
        "SMEMBERS" | "SCARD" => {
            if !rest.is_empty() {
                return Err(replies::arity(name));
            }
            let card = name == "SCARD";
            read(&target, move |held| {
                if card {
                    containers::integer(held.len())
                } else {
                    Reply::Array(held.iter().map(bulk).collect())
                }
            })
        }
        "SISMEMBER" => {
            let [member] = rest else {
                return Err(replies::arity(name));
            };
            let member = values::stored(member);
            read(&target, move |held| {
                Reply::Integer(i64::from(held.contains(&member)))
            })
        }
        "SMISMEMBER" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let asked: Vec<Value> = rest.iter().map(|member| values::stored(member)).collect();
            read(&target, move |held| {
                Reply::Array(
                    asked
                        .iter()
                        .map(|member| Reply::Integer(i64::from(held.contains(member))))
                        .collect(),
                )
            })
        }
        "SRANDMEMBER" => {
            let count = match rest {
                [] => None,
                [count] => Some(integer(count)?),
                _ => return Err(replies::arity(name)),
            };
            read(&target, move |held| random_members(&held, count))
        }
        _ => return Err(replies::unknown(name.as_bytes())),
    };
    Ok(Some(Step::Statements(planned)))
}

/// `SINTER`, `SUNION`, `SDIFF` and `SINTERCARD`: one read of every set named,
/// in one statement so they are one snapshot.
fn combined(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Planned, Reply> {
    let (keys, limit) = if name == "SINTERCARD" {
        let [count, rest @ ..] = arguments else {
            return Err(replies::arity(name));
        };
        let count = usize::try_from(integer(count)?)
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| Reply::error("ERR numkeys should be greater than 0"))?;
        let (keys, options) = rest.split_at(count.min(rest.len()));
        if keys.len() < count {
            return Err(replies::syntax());
        }
        let limit = match options {
            [] => None,
            [word, limit] if word.eq_ignore_ascii_case(b"LIMIT") => {
                usize::try_from(integer(limit)?)
                    .ok()
                    .filter(|limit| *limit > 0)
            }
            _ => return Err(replies::syntax()),
        };
        (keys, limit)
    } else {
        (arguments, None)
    };
    if keys.is_empty() {
        return Err(replies::arity(name));
    }
    let reads: Vec<String> = keys
        .iter()
        .map(|key| format!("GET {}", binder.key(space, key)))
        .collect();
    let answer = name.to_owned();
    Ok(Planned::one(
        format!("RETURN [{}]", reads.join(", ")),
        Box::new(move |outcomes| {
            let mut sets = Vec::new();
            for value in outcomes
                .into_iter()
                .next()
                .map(values_of)
                .unwrap_or_default()
            {
                match members(value) {
                    Ok(held) => sets.push(held),
                    Err(reply) => return reply,
                }
            }
            let mut rest = sets.into_iter();
            let mut result = rest.next().unwrap_or_default();
            for next in rest {
                match answer.as_str() {
                    "SUNION" => result.extend(next),
                    "SDIFF" => result.retain(|member| !next.contains(member)),
                    _ => result.retain(|member| next.contains(member)),
                }
            }
            if answer == "SINTERCARD" {
                return containers::integer(
                    limit.map_or(result.len(), |limit| result.len().min(limit)),
                );
            }
            Reply::Array(result.iter().map(bulk).collect())
        }),
    ))
}

/// `SRANDMEMBER`: one member, `count` distinct ones, or with a negative count
/// that many that may repeat.
fn random_members(held: &BTreeSet<Value>, count: Option<i64>) -> Reply {
    let all: Vec<&Value> = held.iter().collect();
    let Some(count) = count else {
        return all
            .get(containers::random(all.len()))
            .map_or(Reply::Nil, |member| bulk(member));
    };
    let chosen: Vec<&Value> = if count >= 0 {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX).min(all.len());
        let mut left = all.clone();
        (0..wanted)
            .map(|_| left.swap_remove(containers::random(left.len())))
            .collect()
    } else if all.is_empty() {
        Vec::new()
    } else {
        (0..count.unsigned_abs().min(containers::RANDOM_CEILING))
            .filter_map(|_| all.get(containers::random(all.len())).copied())
            .collect()
    };
    Reply::Array(chosen.into_iter().map(bulk).collect())
}
