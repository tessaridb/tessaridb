//! `LMPOP` and `ZMPOP`: elements popped from the first of several keys that
//! holds any, by the store's own pop on each key in turn.

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{bulk, score};
use crate::surface::commands::{Procedure, Step};
use crate::surface::procedures::Against;
use crate::surface::replies;
use crate::surface::running::Failed;
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
    let (keys, front, count) = read(name, arguments)?;
    Ok(Some(Step::Procedure(Procedure::PopFirst {
        keys,
        ranking: name == "ZMPOP",
        front,
        count,
    })))
}

impl Against<'_> {
    /// `LMPOP` / `ZMPOP`: the store's own `LIST POP` or `SCORES POP` on each
    /// key in turn until one gives something — each pop atomic on its key, so
    /// a pop never loses to a write landing on the key between a read and a
    /// write, as a read-then-write-back would under a producer that keeps
    /// pushing (G079 C6).
    pub(crate) async fn pop_first(
        &mut self,
        keys: &[Vec<u8>],
        (ranking, front): (bool, bool),
        count: usize,
    ) -> Reply {
        for key in keys {
            let mut binder = Binder::default();
            let target = binder.key(self.space, key);
            let limit = binder.bind(Value::Number(Number::Integer(
                i64::try_from(count).unwrap_or(i64::MAX),
            )));
            let statement = match (ranking, front) {
                (true, true) => format!("SCORES POP {target} LOWEST LIMIT {limit}"),
                (true, false) => format!("SCORES POP {target} HIGHEST LIMIT {limit}"),
                (false, true) => format!("LIST POP {target} FRONT LIMIT {limit}"),
                (false, false) => format!("LIST POP {target} BACK LIMIT {limit}"),
            };
            let popped = match self.value(statement, binder).await {
                Ok(Value::Array(popped)) => popped,
                Ok(_) => Vec::new(),
                Err(Failed::Refused(refused)) if replies::container(&refused).is_some() => {
                    return replies::container(&refused).unwrap_or_else(replies::syntax);
                }
                Err(failed) => return replies::failed(&failed),
            };
            if popped.is_empty() {
                continue;
            }
            let elements = popped
                .iter()
                .map(|element| match element {
                    // A ranking answers `[member, score]` pairs.
                    Value::Array(pair) if ranking => match pair.as_slice() {
                        [member, Value::Number(at)] => Reply::Array(vec![
                            bulk(member),
                            at.as_float().map_or(Reply::Nil, score),
                        ]),
                        _ => bulk(element),
                    },
                    _ => bulk(element),
                })
                .collect();
            return Reply::Array(vec![Reply::Bulk(key.clone()), Reply::Array(elements)]);
        }
        Reply::NilArray
    }
}
