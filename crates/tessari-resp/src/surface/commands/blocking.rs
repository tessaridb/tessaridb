//! The blocking pops — `BLPOP`, `BRPOP`, `BLMPOP`, `BZPOPMIN`, `BZPOPMAX`,
//! `BZMPOP`, `BLMOVE`, `BRPOPLPUSH` — each its non-blocking form tried again
//! whenever something is committed, until it finds something or its timeout
//! passes. The waiting is the connection's; this plans the form.

use std::time::Duration;

use crate::codec::{Command, Reply};
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::values;

const NAMES: &[&str] = &[
    "BLPOP",
    "BRPOP",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
    "BLMOVE",
    "BRPOPLPUSH",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// How the non-blocking form's reply becomes the blocking command's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Answered as the form answers it.
    AsIs,
    /// `[key, [element]]` as `[key, element]`.
    Element,
    /// `[key, [[member, score]]]` as `[key, member, score]`.
    Scored,
}

impl Shape {
    /// The blocking command's reply to what its form answered.
    pub(crate) fn answer(self, reply: Reply) -> Reply {
        let Reply::Array(parts) = &reply else {
            return reply;
        };
        match (self, parts.as_slice()) {
            (Self::Element, [key, Reply::Array(taken)]) => match taken.as_slice() {
                [element] => Reply::Array(vec![key.clone(), element.clone()]),
                _ => reply,
            },
            (Self::Scored, [key, Reply::Array(taken)]) => match taken.as_slice() {
                [Reply::Array(pair)] => {
                    Reply::Array(std::iter::once(key).chain(pair).cloned().collect())
                }
                _ => reply,
            },
            _ => reply,
        }
    }
}

/// Whether a form's reply found nothing, so the blocking command waits.
pub(crate) const fn found_nothing(reply: &Reply) -> bool {
    matches!(reply, Reply::Nil | Reply::NilArray)
}

/// A timeout in seconds: `0` is no limit.
fn timeout(bytes: &[u8]) -> Result<Option<Duration>, Reply> {
    let seconds = values::float(bytes)
        .filter(|seconds| seconds.is_finite())
        .ok_or_else(|| Reply::error("ERR timeout is not a float or out of range"))?;
    if seconds < 0.0 {
        return Err(Reply::error("ERR timeout is negative"));
    }
    if seconds == 0.0 {
        return Ok(None);
    }
    Duration::try_from_secs_f64(seconds)
        .map(Some)
        .map_err(|_| Reply::error("ERR timeout is not a float or out of range"))
}

/// A word as a command argument.
fn word(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// Plan `name` when it is a blocking pop.
pub(super) fn plan(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let (form, wait, shape): (Command, _, _) = match name {
        // `BLPOP k … timeout` is `LMPOP n k … LEFT`, one element.
        "BLPOP" | "BRPOP" | "BZPOPMIN" | "BZPOPMAX" => {
            let [keys @ .., last] = arguments else {
                return Err(replies::arity(name));
            };
            if keys.is_empty() {
                return Err(replies::arity(name));
            }
            let (form, end, shape) = match name {
                "BLPOP" => ("LMPOP", "LEFT", Shape::Element),
                "BRPOP" => ("LMPOP", "RIGHT", Shape::Element),
                "BZPOPMIN" => ("ZMPOP", "MIN", Shape::Scored),
                _ => ("ZMPOP", "MAX", Shape::Scored),
            };
            let mut command = vec![word(form), word(&keys.len().to_string())];
            command.extend(keys.iter().cloned());
            command.push(word(end));
            (command, timeout(last)?, shape)
        }
        // `BLMPOP timeout numkeys …` is `LMPOP numkeys …`.
        "BLMPOP" | "BZMPOP" => {
            let [wait, rest @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let form = if name == "BLMPOP" { "LMPOP" } else { "ZMPOP" };
            let mut command = vec![word(form)];
            command.extend(rest.iter().cloned());
            (command, timeout(wait)?, Shape::AsIs)
        }
        "BLMOVE" => {
            let [from, to, from_end, to_end, wait] = arguments else {
                return Err(replies::arity(name));
            };
            let command = vec![
                word("LMOVE"),
                from.clone(),
                to.clone(),
                from_end.clone(),
                to_end.clone(),
            ];
            (command, timeout(wait)?, Shape::AsIs)
        }
        _ => {
            let [from, to, wait] = arguments else {
                return Err(replies::arity(name));
            };
            let command = vec![
                word("LMOVE"),
                from.clone(),
                to.clone(),
                word("RIGHT"),
                word("LEFT"),
            ];
            (command, timeout(wait)?, Shape::AsIs)
        }
    };
    Ok(Some(Step::Procedure(Procedure::Block {
        form,
        wait,
        shape,
    })))
}
