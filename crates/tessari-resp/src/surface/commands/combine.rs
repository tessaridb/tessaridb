//! Several keys combined: the sorted-set algebra (`ZUNION`, `ZINTER`, `ZDIFF`,
//! `ZINTERCARD`) read in one statement, and every `*STORE`, which writes the
//! combination to a key while that key still holds what was read.

use std::collections::HashMap;

use tessaridb::Value;

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{
    self, held_ranking, members, ranked, ranked_or_members, score,
};
use crate::surface::commands::ranges::Range;
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::rewrite::{Rewritten, Write};
use crate::surface::statement::{Binder, Planned, values_of};

const NAMES: &[&str] = &[
    "ZUNION",
    "ZINTER",
    "ZDIFF",
    "ZINTERCARD",
    "ZUNIONSTORE",
    "ZINTERSTORE",
    "ZDIFFSTORE",
    "ZRANGESTORE",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// How several sets combine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Combine {
    Union,
    Inter,
    Diff,
}

impl Combine {
    fn of(name: &str) -> Self {
        if name.contains("UNION") {
            Self::Union
        } else if name.contains("INTER") {
            Self::Inter
        } else {
            Self::Diff
        }
    }
}

/// How the scores of one member in several sources fold.
#[derive(Clone, Copy)]
enum Aggregate {
    Sum,
    Min,
    Max,
}

/// What a sorted-set combination asked for after its keys.
struct Ask {
    keys: Vec<Vec<u8>>,
    weights: Vec<f64>,
    aggregate: Aggregate,
    with_scores: bool,
    limit: Option<usize>,
}

/// `numkeys key … [WEIGHTS w …] [AGGREGATE SUM|MIN|MAX] [WITHSCORES] [LIMIT n]`
fn ask(name: &str, arguments: &[Vec<u8>]) -> Result<Ask, Reply> {
    let [count, rest @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let count = usize::try_from(integer(count)?)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| Reply::error("ERR numkeys should be greater than 0"))?;
    if rest.len() < count {
        return Err(replies::syntax());
    }
    let (keys, options) = rest.split_at(count);
    let mut read = Ask {
        keys: keys.to_vec(),
        weights: vec![1.0; count],
        aggregate: Aggregate::Sum,
        with_scores: false,
        limit: None,
    };
    let mut words = options.iter();
    while let Some(word) = words.next() {
        match word.to_ascii_uppercase().as_slice() {
            b"WEIGHTS" if name != "ZDIFF" && name != "ZDIFFSTORE" => {
                for weight in &mut read.weights {
                    let given = words.next().ok_or_else(replies::syntax)?;
                    *weight = crate::surface::values::float(given)
                        .ok_or_else(|| Reply::error("ERR weight value is not a float"))?;
                }
            }
            b"AGGREGATE" if name != "ZDIFF" && name != "ZDIFFSTORE" => {
                read.aggregate = match words
                    .next()
                    .ok_or_else(replies::syntax)?
                    .to_ascii_uppercase()
                    .as_slice()
                {
                    b"SUM" => Aggregate::Sum,
                    b"MIN" => Aggregate::Min,
                    b"MAX" => Aggregate::Max,
                    _ => return Err(replies::syntax()),
                };
            }
            b"WITHSCORES" if !name.ends_with("STORE") && name != "ZINTERCARD" => {
                read.with_scores = true;
            }
            b"LIMIT" if name == "ZINTERCARD" => {
                let limit = integer(words.next().ok_or_else(replies::syntax)?)?;
                read.limit = Some(
                    usize::try_from(limit)
                        .map_err(|_| Reply::error("ERR LIMIT can't be negative"))?,
                );
            }
            _ => return Err(replies::syntax()),
        }
    }
    Ok(read)
}

/// The combination of `sources`, lowest score first.
fn combined(
    how: Combine,
    sources: Vec<Vec<(String, f64)>>,
    weights: &[f64],
    aggregate: Aggregate,
) -> Vec<(String, f64)> {
    let mut sources = sources.into_iter().zip(weights.iter().copied());
    let Some((first, weight)) = sources.next() else {
        return Vec::new();
    };
    let mut held: HashMap<String, f64> = first
        .into_iter()
        .map(|(member, at)| (member, at * weight))
        .collect();
    for (source, weight) in sources {
        let source: HashMap<String, f64> = source.into_iter().collect();
        match how {
            Combine::Diff => held.retain(|member, _| !source.contains_key(member)),
            Combine::Inter => {
                held.retain(|member, _| source.contains_key(member));
                for (member, at) in &mut held {
                    if let Some(other) = source.get(member) {
                        *at = fold(aggregate, *at, other * weight);
                    }
                }
            }
            Combine::Union => {
                for (member, other) in source {
                    let other = other * weight;
                    held.entry(member)
                        .and_modify(|at| *at = fold(aggregate, *at, other))
                        .or_insert(other);
                }
            }
        }
    }
    let mut pairs: Vec<(String, f64)> = held.into_iter().collect();
    pairs.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    pairs
}

fn fold(aggregate: Aggregate, held: f64, other: f64) -> f64 {
    match aggregate {
        Aggregate::Sum => held + other,
        Aggregate::Min => held.min(other),
        Aggregate::Max => held.max(other),
    }
}

/// Plan `name` when it combines keys.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let how = Combine::of(name);
    Ok(Some(match name {
        "SINTERSTORE" | "SUNIONSTORE" | "SDIFFSTORE" => {
            let [destination, sources @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            if sources.is_empty() {
                return Err(replies::arity(name));
            }
            let mut keys = sources.to_vec();
            keys.push(destination.clone());
            let count = sources.len();
            stored(keys, move |held| {
                let mut sets = Vec::with_capacity(count);
                for value in held.iter().take(count) {
                    sets.push(members(value.clone())?);
                }
                let mut sets = sets.into_iter();
                let mut result = sets.next().unwrap_or_default();
                for next in sets {
                    match how {
                        Combine::Union => result.extend(next),
                        Combine::Inter => result.retain(|member| next.contains(member)),
                        Combine::Diff => result.retain(|member| !next.contains(member)),
                    }
                }
                Ok(store(count, Value::Set(result.clone()), result.len()))
            })
        }
        "ZUNIONSTORE" | "ZINTERSTORE" | "ZDIFFSTORE" => {
            let [destination, rest @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let read = ask(name, rest)?;
            let count = read.keys.len();
            let mut keys = read.keys.clone();
            keys.push(destination.clone());
            stored(keys, move |held| {
                let sources = sources(held, count)?;
                let pairs = combined(how, sources, &read.weights, read.aggregate);
                Ok(store(count, held_ranking(&pairs), pairs.len()))
            })
        }
        "ZRANGESTORE" => {
            let [destination, source, rest @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let range = Range::read("ZRANGE", rest, true)?;
            stored(vec![source.clone(), destination.clone()], move |held| {
                let pairs = range.select(ranked(held.first().cloned().unwrap_or(Value::None))?);
                Ok(store(1, held_ranking(&pairs), pairs.len()))
            })
        }
        _ => {
            let read = ask(name, arguments)?;
            let reads: Vec<String> = read
                .keys
                .iter()
                .map(|key| format!("GET {}", binder.key(space, key)))
                .collect();
            let count = read.keys.len();
            let counting = name == "ZINTERCARD";
            Step::Statements(Planned::one(
                format!("RETURN [{}]", reads.join(", ")),
                Box::new(move |outcomes| {
                    let held = outcomes
                        .into_iter()
                        .next()
                        .map(values_of)
                        .unwrap_or_default();
                    let sources = match sources(&held, count) {
                        Ok(sources) => sources,
                        Err(reply) => return reply,
                    };
                    let pairs = combined(how, sources, &read.weights, read.aggregate);
                    if counting {
                        // `ZINTERCARD` answers how many, never which; a limit of
                        // zero is no limit.
                        let limit = read.limit.filter(|limit| *limit > 0);
                        return containers::integer(
                            limit.map_or(pairs.len(), |limit| pairs.len().min(limit)),
                        );
                    }
                    answer(&pairs, read.with_scores)
                }),
            ))
        }
    }))
}

/// The first `count` values as sorted-set sources.
fn sources(held: &[Value], count: usize) -> Result<Vec<Vec<(String, f64)>>, Reply> {
    held.iter()
        .take(count)
        .map(|value| ranked_or_members(value.clone()))
        .collect()
}

/// Members with their scores when asked, as `ZRANGE` answers them.
fn answer(pairs: &[(String, f64)], with_scores: bool) -> Reply {
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

/// The destination — key `at` — written whole, or removed when the result is
/// empty; the reply is how many it holds.
fn store(at: usize, value: Value, count: usize) -> Rewritten {
    Rewritten {
        key: at,
        write: if count == 0 {
            Write::Remove
        } else {
            Write::Hold {
                value,
                keep_expiry: false,
            }
        },
        reply: containers::integer(count),
    }
}

fn stored(
    keys: Vec<Vec<u8>>,
    compute: impl Fn(&[Value]) -> Result<Rewritten, Reply> + Send + Sync + 'static,
) -> Step {
    Step::Procedure(Procedure::Rewrite {
        keys,
        compute: Box::new(compute),
    })
}
