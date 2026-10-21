//! The list commands, as `LIST` statements and reads of the whole list.

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{self, bulk, first, list, stored};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};
use crate::surface::values;

const NAMES: &[&str] = &[
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LLEN",
    "LRANGE",
    "LINDEX",
    "LSET",
    "LREM",
    "LTRIM",
    "LINSERT",
    "LMOVE",
    "RPOPLPUSH",
    "LPOS",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// `FRONT` or `BACK` for a client's `LEFT` or `RIGHT`.
fn end(word: &[u8]) -> Result<&'static str, Reply> {
    if word.eq_ignore_ascii_case(b"LEFT") {
        Ok("FRONT")
    } else if word.eq_ignore_ascii_case(b"RIGHT") {
        Ok("BACK")
    } else {
        Err(replies::syntax())
    }
}

fn length(outcomes: Vec<tessaridb::Outcome>) -> Reply {
    match first(outcomes) {
        Value::Number(Number::Integer(length)) => Reply::Integer(length),
        _ => Reply::Integer(0),
    }
}

/// A read of the whole list, answered by `answer`.
fn read(target: &str, answer: impl FnOnce(Vec<Value>) -> Reply + Send + 'static) -> Planned {
    Planned::one(
        format!("GET {target}"),
        Box::new(move |outcomes| match list(first(outcomes)) {
            Ok(items) => answer(items),
            Err(reply) => reply,
        }),
    )
}

/// Plan `name` when it is a list command.
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
        "LPUSH" | "RPUSH" | "LPUSHX" | "RPUSHX" => {
            if rest.is_empty() {
                return Err(replies::arity(name));
            }
            let side = if name.starts_with('L') {
                "FRONT"
            } else {
                "BACK"
            };
            let only = if name.ends_with('X') {
                " IF PRESENT"
            } else {
                ""
            };
            let values = stored(rest, binder);
            Planned::one(
                format!("LIST PUSH {target} {side} {values}{only}"),
                Box::new(length),
            )
        }
        "LPOP" | "RPOP" => {
            let side = if name == "LPOP" { "FRONT" } else { "BACK" };
            match rest {
                [] => Planned::one(
                    format!("LIST POP {target} {side}"),
                    Box::new(|outcomes| bulk(&first(outcomes))),
                ),
                [count] => {
                    let count = integer(count)?;
                    if count < 0 {
                        return Err(Reply::error("ERR value is out of range, must be positive"));
                    }
                    let bound = binder.bind(Value::Number(Number::Integer(count)));
                    Planned::one(
                        format!("LIST POP {target} {side} LIMIT {bound}"),
                        Box::new(move |outcomes| match first(outcomes) {
                            Value::Array(items) if items.is_empty() && count > 0 => Reply::Nil,
                            Value::Array(items) => Reply::Array(items.iter().map(bulk).collect()),
                            _ => Reply::Nil,
                        }),
                    )
                }
                _ => return Err(replies::arity(name)),
            }
        }
        "LSET" => {
            let [index, value] = rest else {
                return Err(replies::arity(name));
            };
            let index = binder.bind(Value::Number(Number::Integer(integer(index)?)));
            let value = binder.bind(values::stored(value));
            Planned::one(
                format!("LIST SET {target} AT {index} = {value}"),
                Box::new(|_| Reply::OK),
            )
        }
        "LREM" => {
            let [count, value] = rest else {
                return Err(replies::arity(name));
            };
            let count = integer(count)?;
            let value = binder.bind(values::stored(value));
            let bound = match count {
                0 => String::new(),
                _ => {
                    let limit = binder.bind(Value::Number(Number::Integer(
                        i64::try_from(count.unsigned_abs()).unwrap_or(i64::MAX),
                    )));
                    let back = if count < 0 { " FROM BACK" } else { "" };
                    format!(" LIMIT {limit}{back}")
                }
            };
            Planned::one(
                format!("LIST REMOVE {target} {value}{bound}"),
                Box::new(length),
            )
        }
        "LTRIM" => {
            let [start, stop] = rest else {
                return Err(replies::arity(name));
            };
            let start = binder.bind(Value::Number(Number::Integer(integer(start)?)));
            let stop = binder.bind(Value::Number(Number::Integer(integer(stop)?)));
            Planned::one(
                format!("LIST TRIM {target} FROM {start} TO {stop}"),
                Box::new(|_| Reply::OK),
            )
        }
        "LINSERT" => {
            let [side, pivot, value] = rest else {
                return Err(replies::arity(name));
            };
            let side = if side.eq_ignore_ascii_case(b"BEFORE") {
                "BEFORE"
            } else if side.eq_ignore_ascii_case(b"AFTER") {
                "AFTER"
            } else {
                return Err(replies::syntax());
            };
            let pivot = binder.bind(values::stored(pivot));
            let value = binder.bind(values::stored(value));
            Planned::one(
                format!("LIST INSERT {target} {value} {side} {pivot}"),
                Box::new(length),
            )
        }
        "LMOVE" | "RPOPLPUSH" => {
            let (to, from_side, to_side) = match (name, rest) {
                ("LMOVE", [to, from, into]) => (to, end(from)?, end(into)?),
                ("RPOPLPUSH", [to]) => (to, "BACK", "FRONT"),
                _ => return Err(replies::arity(name)),
            };
            let to = binder.key(space, to);
            Planned::one(
                format!("LIST MOVE {target} {from_side} TO {to} {to_side}"),
                Box::new(|outcomes| bulk(&first(outcomes))),
            )
        }
        _ => reads(name, rest, &target)?,
    };
    Ok(Some(Step::Statements(planned)))
}

/// The commands that read the list and change nothing.
fn reads(name: &str, rest: &[Vec<u8>], target: &str) -> Result<Planned, Reply> {
    Ok(match name {
        "LLEN" => {
            if !rest.is_empty() {
                return Err(replies::arity(name));
            }
            read(target, |items| containers::integer(items.len()))
        }
        "LRANGE" => {
            let [start, stop] = rest else {
                return Err(replies::arity(name));
            };
            let (start, stop) = (integer(start)?, integer(stop)?);
            read(target, move |items| {
                Reply::Array(match containers::span(items.len(), start, stop) {
                    Some((from, to)) => items
                        .get(from..=to)
                        .unwrap_or(&[])
                        .iter()
                        .map(bulk)
                        .collect(),
                    None => Vec::new(),
                })
            })
        }
        "LINDEX" => {
            let [index] = rest else {
                return Err(replies::arity(name));
            };
            let index = integer(index)?;
            read(target, move |items| {
                containers::span(items.len(), index, index)
                    .and_then(|(at, _)| items.get(at))
                    .map_or(Reply::Nil, bulk)
            })
        }
        "LPOS" => {
            let [element, options @ ..] = rest else {
                return Err(replies::arity(name));
            };
            let wanted = values::stored(element);
            let options = positions(options)?;
            read(target, move |items| options.answer(&items, &wanted))
        }
        _ => return Err(replies::unknown(name.as_bytes())),
    })
}

/// What `LPOS … [RANK r] [COUNT n] [MAXLEN m]` asked for.
struct Positions {
    rank: i64,
    count: Option<usize>,
    longest: usize,
}

fn positions(options: &[Vec<u8>]) -> Result<Positions, Reply> {
    let mut read = Positions {
        rank: 1,
        count: None,
        longest: usize::MAX,
    };
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        let value = integer(rest.next().ok_or_else(replies::syntax)?)?;
        match option.to_ascii_uppercase().as_slice() {
            b"RANK" if value != 0 => read.rank = value,
            b"COUNT" if value >= 0 => {
                read.count = Some(usize::try_from(value).unwrap_or(usize::MAX));
            }
            b"MAXLEN" if value >= 0 => {
                read.longest = match usize::try_from(value).unwrap_or(usize::MAX) {
                    0 => usize::MAX,
                    longest => longest,
                };
            }
            _ => return Err(replies::syntax()),
        }
    }
    Ok(read)
}

impl Positions {
    fn answer(&self, items: &[Value], wanted: &Value) -> Reply {
        let indexes: Vec<usize> = (0..items.len()).collect();
        let order: Box<dyn Iterator<Item = &usize>> = if self.rank > 0 {
            Box::new(indexes.iter())
        } else {
            Box::new(indexes.iter().rev())
        };
        let skip =
            usize::try_from(self.rank.unsigned_abs().saturating_sub(1)).unwrap_or(usize::MAX);
        let found: Vec<usize> = order
            .take(self.longest)
            .filter(|at| items.get(**at) == Some(wanted))
            .skip(skip)
            .take(match self.count {
                None => 1,
                Some(0) => usize::MAX,
                Some(count) => count,
            })
            .copied()
            .collect();
        match self.count {
            None => found
                .first()
                .map_or(Reply::Nil, |at| containers::integer(*at)),
            Some(_) => Reply::Array(found.into_iter().map(containers::integer).collect()),
        }
    }
}
