//! `LMPOP` and `ZMPOP`: elements popped from the first of several keys that
//! holds any, written back while that key still holds what was read.

use tessaridb::Value;

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{held_ranking, list, ranked, score};
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::rewrite::{Rewritten, Write};
use crate::surface::statement::Binder;

const NAMES: &[&str] = &["LMPOP", "ZMPOP"];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// `numkeys key … LEFT|RIGHT|MIN|MAX [COUNT n]`: the keys, which end, and how many.
fn read(name: &str, arguments: &[Vec<u8>]) -> Result<(Vec<Vec<u8>>, bool, usize), Reply> {
    let [count, rest @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let count = usize::try_from(integer(count)?)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| Reply::error("ERR numkeys should be greater than 0"))?;
    if rest.len() <= count {
        return Err(replies::syntax());
    }
    let (keys, options) = rest.split_at(count);
    let [end, more @ ..] = options else {
        return Err(replies::syntax());
    };
    let front = match (name, end.to_ascii_uppercase().as_slice()) {
        ("LMPOP", b"LEFT") | ("ZMPOP", b"MIN") => true,
        ("LMPOP", b"RIGHT") | ("ZMPOP", b"MAX") => false,
        _ => return Err(replies::syntax()),
    };
    let taken = match more {
        [] => 1,
        [word, taken] if word.eq_ignore_ascii_case(b"COUNT") => usize::try_from(integer(taken)?)
            .ok()
            .filter(|taken| *taken > 0)
            .ok_or_else(|| Reply::error("ERR count should be greater than 0"))?,
        _ => return Err(replies::syntax()),
    };
    Ok((keys.to_vec(), front, taken))
}

/// Plan `name` when it pops from the first of several keys.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    _space: &Space,
    _binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let (keys, front, taken) = read(name, arguments)?;
    let names = keys.clone();
    let ranking = name == "ZMPOP";
    let compute = move |held: &[Value]| -> Result<Rewritten, Reply> {
        for (at, value) in held.iter().enumerate() {
            let key = Reply::Bulk(names.get(at).cloned().unwrap_or_default());
            if ranking {
                let mut pairs = ranked(value.clone())?;
                if pairs.is_empty() {
                    continue;
                }
                let popped: Vec<(String, f64)> = if front {
                    pairs.drain(..taken.min(pairs.len())).collect()
                } else {
                    let keep = pairs.len().saturating_sub(taken);
                    pairs.split_off(keep).into_iter().rev().collect()
                };
                let elements = popped
                    .into_iter()
                    .map(|(member, at)| Reply::Array(vec![Reply::text(member), score(at)]))
                    .collect();
                return Ok(rewritten(
                    at,
                    (!pairs.is_empty()).then(|| held_ranking(&pairs)),
                    key,
                    elements,
                ));
            }
            let mut items = list(value.clone())?;
            if items.is_empty() {
                continue;
            }
            let popped: Vec<Value> = if front {
                items.drain(..taken.min(items.len())).collect()
            } else {
                let keep = items.len().saturating_sub(taken);
                items.split_off(keep).into_iter().rev().collect()
            };
            let elements = popped.iter().map(super::containers::bulk).collect();
            return Ok(rewritten(
                at,
                (!items.is_empty()).then_some(Value::Array(items)),
                key,
                elements,
            ));
        }
        Ok(Rewritten {
            key: 0,
            write: Write::Nothing,
            reply: Reply::NilArray,
        })
    };
    Ok(Some(Step::Procedure(Procedure::Rewrite {
        keys,
        compute: Box::new(compute),
    })))
}

/// The key at `at` left holding `rest`, or removed when nothing is left.
fn rewritten(at: usize, rest: Option<Value>, key: Reply, elements: Vec<Reply>) -> Rewritten {
    Rewritten {
        key: at,
        write: rest.map_or(Write::Remove, |value| Write::Hold {
            value,
            keep_expiry: true,
        }),
        reply: Reply::Array(vec![key, Reply::Array(elements)]),
    }
}
