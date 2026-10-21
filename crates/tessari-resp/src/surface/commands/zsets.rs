//! The sorted-set commands, as `SCORES` statements and reads of the whole
//! ranking, which the store already holds in score order.

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{self, first, ranked, score, text};
use crate::surface::commands::ranges::{Bound, Lex, Range};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};
use crate::surface::values;

const NAMES: &[&str] = &[
    "ZADD",
    "ZINCRBY",
    "ZREM",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGEBYLEX",
    "ZLEXCOUNT",
    "ZRANK",
    "ZREVRANK",
    "ZPOPMIN",
    "ZPOPMAX",
    "ZRANDMEMBER",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// A read of the whole ranking, lowest first, answered by `answer`.
fn read(
    target: &str,
    answer: impl FnOnce(Vec<(String, f64)>) -> Reply + Send + 'static,
) -> Planned {
    Planned::one(
        format!("GET {target}"),
        Box::new(move |outcomes| match ranked(first(outcomes)) {
            Ok(pairs) => answer(pairs),
            Err(reply) => reply,
        }),
    )
}

/// Members, or each member paired with its score when asked.
fn listed(pairs: &[(String, f64)], with_scores: bool) -> Reply {
    if with_scores {
        return Reply::Pairs(
            pairs
                .iter()
                .map(|(member, at)| (Reply::text(member.clone()), score(*at)))
                .collect(),
        );
    }
    Reply::Array(
        pairs
            .iter()
            .map(|(member, _)| Reply::text(member.clone()))
            .collect(),
    )
}

/// A score a client sent.
fn sent(bytes: &[u8]) -> Result<Value, Reply> {
    values::float(bytes)
        .map(|score| Value::Number(Number::float(score)))
        .ok_or_else(replies::not_a_float)
}

/// Plan `name` when it is a sorted-set command.
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
        "ZADD" => add(rest, &target, binder)?,
        "ZINCRBY" => {
            let [by, member] = rest else {
                return Err(replies::arity(name));
            };
            increment(&target, by, member, binder)?
        }
        "ZREM" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let members = rest
                .iter()
                .map(|member| Ok(binder.bind(Value::String(text(member)?))))
                .collect::<Result<Vec<_>, Reply>>()?
                .join(", ");
            Planned::one(
                format!("SCORES REMOVE {target} {members}"),
                Box::new(|outcomes| match first(outcomes) {
                    Value::Number(Number::Integer(removed)) => Reply::Integer(removed),
                    _ => Reply::Integer(0),
                }),
            )
        }
        "ZPOPMIN" | "ZPOPMAX" => {
            let side = if name == "ZPOPMIN" {
                "LOWEST"
            } else {
                "HIGHEST"
            };
            let limit = match rest {
                [] => String::new(),
                [count] => {
                    let count = integer(count)?;
                    if count < 0 {
                        return Err(Reply::error("ERR value is out of range, must be positive"));
                    }
                    format!(
                        " LIMIT {}",
                        binder.bind(Value::Number(Number::Integer(count)))
                    )
                }
                _ => return Err(replies::syntax()),
            };
            let counted = !limit.is_empty();
            Planned::one(
                format!("SCORES POP {target} {side}{limit}"),
                Box::new(move |outcomes| {
                    let Value::Array(pairs) = first(outcomes) else {
                        return Reply::Array(Vec::new());
                    };
                    let mut popped = Vec::new();
                    for pair in pairs {
                        if let Value::Array(pair) = pair
                            && let [member, Value::Number(at)] = pair.as_slice()
                        {
                            popped.push((
                                containers::bulk(member),
                                at.as_float().map_or(Reply::Nil, score),
                            ));
                        }
                    }
                    // With no count the one pair is answered flat, as RESP3 has it.
                    match popped.as_slice() {
                        [(member, at)] if !counted => {
                            Reply::Array(vec![member.clone(), at.clone()])
                        }
                        _ => Reply::Pairs(popped),
                    }
                }),
            )
        }
        _ => reads(name, rest, &target)?,
    };
    Ok(Some(Step::Statements(planned)))
}

/// `ZADD k [NX|XX] [GT|LT] [CH] [INCR] score member …`
fn add(rest: &[Vec<u8>], target: &str, binder: &mut Binder) -> Result<Planned, Reply> {
    let (mut presence, mut compare, mut changed, mut incr) = (None, None, false, false);
    let mut at = 0;
    while let Some(word) = rest.get(at) {
        match word.to_ascii_uppercase().as_slice() {
            b"NX" | b"XX" => {
                let asked = if word.eq_ignore_ascii_case(b"NX") {
                    "IF ABSENT"
                } else {
                    "IF PRESENT"
                };
                if presence.is_some_and(|held| held != asked) {
                    return Err(Reply::error(
                        "ERR XX and NX options at the same time are not compatible",
                    ));
                }
                presence = Some(asked);
            }
            b"GT" => compare = Some("IF GREATER"),
            b"LT" => compare = Some("IF LESS"),
            b"CH" => changed = true,
            b"INCR" => incr = true,
            _ => break,
        }
        at = at.saturating_add(1);
    }
    let pairs = rest.get(at..).unwrap_or(&[]);
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Err(replies::syntax());
    }
    if presence == Some("IF ABSENT") && compare.is_some() {
        return Err(Reply::error(
            "ERR GT, LT, and/or NX options at the same time are not compatible",
        ));
    }
    if incr {
        let [by, member] = pairs else {
            return Err(Reply::error(
                "ERR INCR option supports a single increment-element pair",
            ));
        };
        if presence.is_some() || compare.is_some() {
            return Err(Reply::error(
                "ERR INCR with NX, XX, GT or LT is not served by this server",
            ));
        }
        return increment(target, by, member, binder);
    }
    let mut written = Vec::new();
    for pair in pairs.chunks(2) {
        let [at, member] = pair else {
            return Err(replies::syntax());
        };
        let member = binder.bind(Value::String(text(member)?));
        let at = binder.bind(sent(at)?);
        written.push(format!("{member} = {at}"));
    }
    let mut statement = format!("SCORES ADD {target} {}", written.join(", "));
    for clause in [presence, compare].into_iter().flatten() {
        statement.push(' ');
        statement.push_str(clause);
    }
    if changed {
        statement.push_str(" CHANGED");
    }
    Ok(Planned::one(
        statement,
        Box::new(|outcomes| match first(outcomes) {
            Value::Number(Number::Integer(count)) => Reply::Integer(count),
            _ => Reply::Integer(0),
        }),
    ))
}

/// `ZINCRBY` and `ZADD … INCR`: one `SCORES INCR`.
fn increment(
    target: &str,
    by: &[u8],
    member: &[u8],
    binder: &mut Binder,
) -> Result<Planned, Reply> {
    let member = binder.bind(Value::String(text(member)?));
    let by = binder.bind(sent(by)?);
    Ok(Planned::one(
        format!("SCORES INCR {target} {member} BY {by}"),
        Box::new(|outcomes| match first(outcomes) {
            Value::Number(number) => number.as_float().map_or(Reply::Nil, score),
            _ => Reply::Nil,
        }),
    ))
}

/// The commands that read the ranking and change nothing.
fn reads(name: &str, rest: &[Vec<u8>], target: &str) -> Result<Planned, Reply> {
    Ok(match name {
        "ZSCORE" => {
            let [member] = rest else {
                return Err(replies::arity(name));
            };
            let member = text(member)?;
            read(target, move |pairs| {
                pairs
                    .iter()
                    .find(|(held, _)| *held == member)
                    .map_or(Reply::Nil, |(_, at)| score(*at))
            })
        }
        "ZMSCORE" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let asked = rest
                .iter()
                .map(|member| text(member))
                .collect::<Result<Vec<_>, _>>()?;
            read(target, move |pairs| {
                Reply::Array(
                    asked
                        .iter()
                        .map(|member| {
                            pairs
                                .iter()
                                .find(|(held, _)| held == member)
                                .map_or(Reply::Nil, |(_, at)| score(*at))
                        })
                        .collect(),
                )
            })
        }
        "ZCARD" => {
            if !rest.is_empty() {
                return Err(replies::arity(name));
            }
            read(target, |pairs| containers::integer(pairs.len()))
        }
        "ZCOUNT" => {
            let [low, high] = rest else {
                return Err(replies::arity(name));
            };
            let (low, high) = (Bound::read(low)?, Bound::read(high)?);
            read(target, move |pairs| {
                containers::integer(
                    pairs
                        .iter()
                        .filter(|(_, at)| low.below(*at) && high.above(*at))
                        .count(),
                )
            })
        }
        "ZRANK" | "ZREVRANK" => {
            let (member, with_score) = match rest {
                [member] => (member, false),
                [member, with] if with.eq_ignore_ascii_case(b"WITHSCORE") => (member, true),
                _ => return Err(replies::syntax()),
            };
            let member = text(member)?;
            let reverse = name == "ZREVRANK";
            read(target, move |pairs| {
                let Some(at) = pairs.iter().position(|(held, _)| *held == member) else {
                    return Reply::Nil;
                };
                let rank = if reverse {
                    pairs.len().saturating_sub(at).saturating_sub(1)
                } else {
                    at
                };
                if with_score {
                    let held = pairs.get(at).map_or(Reply::Nil, |(_, at)| score(*at));
                    Reply::Array(vec![containers::integer(rank), held])
                } else {
                    containers::integer(rank)
                }
            })
        }
        "ZRANDMEMBER" => {
            let (count, with_scores) = match rest {
                [] => (None, false),
                [count] => (Some(integer(count)?), false),
                [count, with] if with.eq_ignore_ascii_case(b"WITHSCORES") => {
                    (Some(integer(count)?), true)
                }
                _ => return Err(replies::syntax()),
            };
            read(target, move |pairs| {
                random_members(&pairs, count, with_scores)
            })
        }
        "ZLEXCOUNT" => {
            let [low, high] = rest else {
                return Err(replies::arity(name));
            };
            let (low, high) = (Lex::read(low)?, Lex::read(high)?);
            read(target, move |pairs| {
                containers::integer(
                    pairs
                        .iter()
                        .filter(|(member, _)| low.below(member) && high.above(member))
                        .count(),
                )
            })
        }
        _ => {
            let range = Range::read(name, rest, false)?;
            read(target, move |pairs| {
                listed(&range.select(pairs), range.with_scores)
            })
        }
    })
}

/// `ZRANDMEMBER`: one member, `count` distinct ones, or with a negative count
/// that many that may repeat.
fn random_members(pairs: &[(String, f64)], count: Option<i64>, with_scores: bool) -> Reply {
    let Some(count) = count else {
        return pairs
            .get(containers::random(pairs.len()))
            .map_or(Reply::Nil, |(member, _)| Reply::text(member.clone()));
    };
    let chosen: Vec<(String, f64)> = if count >= 0 {
        let wanted = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(pairs.len());
        let mut left = pairs.to_vec();
        (0..wanted)
            .map(|_| left.swap_remove(containers::random(left.len())))
            .collect()
    } else if pairs.is_empty() {
        Vec::new()
    } else {
        (0..count.unsigned_abs().min(containers::RANDOM_CEILING))
            .filter_map(|_| pairs.get(containers::random(pairs.len())).cloned())
            .collect()
    };
    listed(&chosen, with_scores)
}
