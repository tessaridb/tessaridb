//! The string commands, as statements on the connection's space.

use tessaridb::{Number, Outcome, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::arguments::{Expiry, expiry, expiry_value, integer, now_millis};
use crate::surface::commands::change::Change;
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned, value_of, values_of};
use crate::surface::values;

const NAMES: &[&str] = &[
    "GET",
    "SET",
    "SETNX",
    "SETEX",
    "PSETEX",
    "GETSET",
    "GETDEL",
    "GETEX",
    "MGET",
    "MSET",
    "MSETNX",
    "STRLEN",
    "GETRANGE",
    "SUBSTR",
    "APPEND",
    "SETRANGE",
    "INCR",
    "DECR",
    "INCRBY",
    "DECRBY",
    "INCRBYFLOAT",
    "LCS",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The reply for a string value: its bytes, nil for none, an error for a
/// kind no string stands for.
pub(crate) fn string_reply(value: &Value) -> Reply {
    if *value == Value::None {
        return Reply::Nil;
    }
    values::rendered(value).map_or_else(replies::wrong_type, Reply::Bulk)
}

fn first(outcomes: Vec<Outcome>) -> Value {
    outcomes.into_iter().next().map_or(Value::None, value_of)
}

fn last(outcomes: Vec<Outcome>) -> Value {
    outcomes.into_iter().last().map_or(Value::None, value_of)
}

fn arity(name: &str, arguments: &[Vec<u8>], wanted: usize) -> Result<(), Reply> {
    if arguments.len() == wanted {
        Ok(())
    } else {
        Err(replies::arity(name))
    }
}

fn invalid_expire(name: &str) -> Reply {
    Reply::error(format!(
        "ERR invalid expire time in '{}' command",
        name.to_ascii_lowercase()
    ))
}

/// A positive expiry from `count`, or the reply refusing it.
fn positive(name: &str, count: i64, millis: bool, absolute: bool) -> Result<Expiry, Reply> {
    if count <= 0 {
        return Err(invalid_expire(name));
    }
    expiry(count, millis, absolute).ok_or_else(|| invalid_expire(name))
}

/// What `SET`'s options asked for.
#[derive(Default)]
struct SetOptions {
    condition: Option<&'static str>,
    expire: Option<Expiry>,
    keep: bool,
    get: bool,
}

fn set_options(name: &str, options: &[Vec<u8>]) -> Result<SetOptions, Reply> {
    let mut read = SetOptions::default();
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        let word = option.to_ascii_uppercase();
        match word.as_slice() {
            b"NX" | b"XX" if read.condition.is_none() => {
                read.condition = Some(if word == b"NX" {
                    "IF ABSENT"
                } else {
                    "IF PRESENT"
                });
            }
            b"GET" => read.get = true,
            b"KEEPTTL" if read.expire.is_none() => read.keep = true,
            b"EX" | b"PX" | b"EXAT" | b"PXAT" if read.expire.is_none() && !read.keep => {
                let count = integer(rest.next().ok_or_else(replies::syntax)?)?;
                let millis = word.starts_with(b"P");
                read.expire = Some(positive(name, count, millis, word.ends_with(b"AT"))?);
            }
            _ => return Err(replies::syntax()),
        }
    }
    Ok(read)
}

/// `SET target = value …` with its condition and expiry.
fn set_statement(target: &str, value: &str, options: &SetOptions, binder: &mut Binder) -> String {
    let mut statement = format!("SET {target} = {value}");
    if let Some(condition) = options.condition {
        statement.push(' ');
        statement.push_str(condition);
    }
    if let Some(at) = options.expire {
        let bound = binder.bind(expiry_value(at));
        statement.push_str(&format!(" EXPIRE {bound}"));
    } else if options.keep {
        // `NULL` for a key that never expires and for one that is not there:
        // both write a key that never expires (the `??` makes NONE a NULL).
        statement.push_str(&format!(" EXPIRE (TTL {target} ?? NULL)"));
    }
    statement
}

/// `SET` and the commands that are one of its forms.
fn set(
    name: &str,
    key: &[u8],
    value: &[u8],
    options: &SetOptions,
    space: &Space,
    binder: &mut Binder,
) -> Planned {
    let target = binder.key(space, key);
    let bound = binder.bind(values::stored(value));
    let statement = set_statement(&target, &bound, options, binder);
    if options.get {
        let old = binder.variable();
        return Planned::many(
            vec![
                format!("LET {old} = GET {target}"),
                statement,
                format!("RETURN {old}"),
            ],
            Box::new(|outcomes| string_reply(&last(outcomes))),
        );
    }
    let conditional = options.condition.is_some();
    let setnx = name == "SETNX";
    Planned::one(
        statement,
        Box::new(move |outcomes| match first(outcomes) {
            Value::Bool(wrote) if setnx => Reply::Integer(i64::from(wrote)),
            Value::Bool(true) => Reply::OK,
            Value::Bool(false) => Reply::Nil,
            _ if conditional => Reply::Nil,
            _ => Reply::OK,
        }),
    )
}

/// A slice of `bytes` by the command's inclusive, possibly negative, bounds.
fn range(bytes: &[u8], start: i64, end: i64) -> &[u8] {
    let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let at = |index: i64| {
        if index < 0 {
            length.saturating_add(index).max(0)
        } else {
            index
        }
    };
    let (start, end) = (at(start), at(end).min(length.saturating_sub(1)));
    if start > end || length == 0 {
        return &[];
    }
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return &[];
    };
    bytes.get(start..=end).unwrap_or(&[])
}

fn increment(target: &str, by: i64, binder: &mut Binder) -> Planned {
    let bound = binder.bind(Value::Number(Number::Integer(by)));
    Planned::one(
        format!("INCR {target} BY {bound}"),
        Box::new(|outcomes| match first(outcomes) {
            Value::Number(Number::Integer(next)) => Reply::Integer(next),
            _ => replies::not_an_integer(),
        }),
    )
    .refusing_invalid_as(replies::not_an_integer())
}

/// Plan `name` when it is a string command.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    let planned = match name {
        "GET" => {
            arity(name, arguments, 1)?;
            let target = binder.key(space, &arguments[0]);
            Planned::one(
                format!("GET {target}"),
                Box::new(|outcomes| string_reply(&first(outcomes))),
            )
        }
        "SET" => {
            let [key, value, options @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let options = set_options(name, options)?;
            set(name, key, value, &options, space, binder)
        }
        "SETNX" => {
            arity(name, arguments, 2)?;
            let options = SetOptions {
                condition: Some("IF ABSENT"),
                ..SetOptions::default()
            };
            set(name, &arguments[0], &arguments[1], &options, space, binder)
        }
        "SETEX" | "PSETEX" => {
            arity(name, arguments, 3)?;
            let count = integer(&arguments[1])?;
            let options = SetOptions {
                expire: Some(positive(name, count, name == "PSETEX", false)?),
                ..SetOptions::default()
            };
            set(name, &arguments[0], &arguments[2], &options, space, binder)
        }
        "GETSET" => {
            arity(name, arguments, 2)?;
            let options = SetOptions {
                get: true,
                ..SetOptions::default()
            };
            set(name, &arguments[0], &arguments[1], &options, space, binder)
        }
        "GETDEL" => {
            arity(name, arguments, 1)?;
            let target = binder.key(space, &arguments[0]);
            Planned::one(
                format!("DELETE {target} RETURN BEFORE"),
                Box::new(|outcomes| string_reply(&first(outcomes))),
            )
        }
        "GETEX" => {
            let [key, options @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let target = binder.key(space, key);
            let then = match options {
                [] => None,
                [persist] if persist.eq_ignore_ascii_case(b"PERSIST") => {
                    Some(format!("PERSIST {target}"))
                }
                [unit, count] => {
                    let unit = unit.to_ascii_uppercase();
                    if !matches!(unit.as_slice(), b"EX" | b"PX" | b"EXAT" | b"PXAT") {
                        return Err(replies::syntax());
                    }
                    let at = positive(
                        name,
                        integer(count)?,
                        unit.starts_with(b"P"),
                        unit.ends_with(b"AT"),
                    )?;
                    let bound = binder.bind(expiry_value(at));
                    Some(format!("EXPIRE {target} {bound}"))
                }
                _ => return Err(replies::syntax()),
            };
            match then {
                None => Planned::one(
                    format!("GET {target}"),
                    Box::new(|outcomes| string_reply(&first(outcomes))),
                ),
                Some(then) => {
                    let old = binder.variable();
                    Planned::many(
                        vec![
                            format!("LET {old} = GET {target}"),
                            then,
                            format!("RETURN {old}"),
                        ],
                        Box::new(|outcomes| string_reply(&last(outcomes))),
                    )
                }
            }
        }
        "MGET" => {
            if arguments.is_empty() {
                return Err(replies::arity(name));
            }
            let reads: Vec<String> = arguments
                .iter()
                .map(|key| format!("GET {}", binder.key(space, key)))
                .collect();
            Planned::one(
                format!("RETURN [{}]", reads.join(", ")),
                Box::new(|outcomes| {
                    Reply::Array(
                        outcomes
                            .into_iter()
                            .next()
                            .map(values_of)
                            .unwrap_or_default()
                            .iter()
                            .map(|value| match string_reply(value) {
                                Reply::Error(_) => Reply::Nil,
                                reply => reply,
                            })
                            .collect(),
                    )
                }),
            )
        }
        "MSET" => {
            if arguments.is_empty() || !arguments.len().is_multiple_of(2) {
                return Err(replies::arity(name));
            }
            let statements = arguments
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| {
                    let target = binder.key(space, key);
                    let value = binder.bind(values::stored(value));
                    format!("SET {target} = {value}")
                })
                .collect();
            Planned::many(statements, Box::new(|_| Reply::OK))
        }
        "MSETNX" => {
            if arguments.is_empty() || !arguments.len().is_multiple_of(2) {
                return Err(replies::arity(name));
            }
            let pairs = arguments
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| (key.clone(), value.clone()))
                .collect();
            return Ok(Some(Step::Procedure(Procedure::SetIfNoneExist(pairs))));
        }
        "STRLEN" => {
            arity(name, arguments, 1)?;
            let target = binder.key(space, &arguments[0]);
            Planned::one(
                format!("GET {target}"),
                Box::new(|outcomes| match string_reply(&first(outcomes)) {
                    Reply::Bulk(bytes) => Reply::Integer(i64::try_from(bytes.len()).unwrap_or(0)),
                    Reply::Nil => Reply::Integer(0),
                    error => error,
                }),
            )
        }
        "GETRANGE" | "SUBSTR" => {
            arity(name, arguments, 3)?;
            let (start, end) = (integer(&arguments[1])?, integer(&arguments[2])?);
            let target = binder.key(space, &arguments[0]);
            Planned::one(
                format!("GET {target}"),
                Box::new(move |outcomes| match string_reply(&first(outcomes)) {
                    Reply::Bulk(bytes) => Reply::Bulk(range(&bytes, start, end).to_vec()),
                    Reply::Nil => Reply::Bulk(Vec::new()),
                    error => error,
                }),
            )
        }
        "APPEND" => {
            arity(name, arguments, 2)?;
            return Ok(Some(change(
                &arguments[0],
                Change::Append(arguments[1].clone()),
            )));
        }
        "SETRANGE" => {
            arity(name, arguments, 3)?;
            let offset = usize::try_from(integer(&arguments[1])?)
                .map_err(|_| Reply::error("ERR offset is out of range"))?;
            let bytes = arguments[2].clone();
            return Ok(Some(change(
                &arguments[0],
                Change::SetRange { offset, bytes },
            )));
        }
        "INCRBYFLOAT" => {
            arity(name, arguments, 2)?;
            let by = values::float(&arguments[1]).ok_or_else(replies::not_a_float)?;
            return Ok(Some(change(&arguments[0], Change::IncrementFloat(by))));
        }
        "INCR" | "DECR" => {
            arity(name, arguments, 1)?;
            let target = binder.key(space, &arguments[0]);
            increment(&target, if name == "INCR" { 1 } else { -1 }, binder)
        }
        "INCRBY" | "DECRBY" => {
            arity(name, arguments, 2)?;
            let by = integer(&arguments[1])?;
            let by = if name == "INCRBY" {
                by
            } else {
                by.checked_neg()
                    .ok_or_else(|| Reply::error("ERR decrement would overflow"))?
            };
            let target = binder.key(space, &arguments[0]);
            increment(&target, by, binder)
        }
        "LCS" => {
            let [first_key, second_key, options @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let only_length = match options {
                [] => false,
                [option] if option.eq_ignore_ascii_case(b"LEN") => true,
                _ => return Err(replies::syntax()),
            };
            let reads = format!(
                "RETURN [GET {}, GET {}]",
                binder.key(space, first_key),
                binder.key(space, second_key)
            );
            Planned::one(
                reads,
                Box::new(move |outcomes| {
                    let read: Vec<Reply> = outcomes
                        .into_iter()
                        .next()
                        .map(values_of)
                        .unwrap_or_default()
                        .iter()
                        .map(string_reply)
                        .collect();
                    let bytes = |reply: &Reply| match reply {
                        Reply::Bulk(bytes) => Ok(bytes.clone()),
                        Reply::Nil => Ok(Vec::new()),
                        error => Err(error.clone()),
                    };
                    match (read.first().map(bytes), read.get(1).map(bytes)) {
                        (Some(Ok(a)), Some(Ok(b))) => {
                            super::keys::longest_common(&a, &b, only_length)
                        }
                        (Some(Err(error)), _) | (_, Some(Err(error))) => error,
                        _ => Reply::Bulk(Vec::new()),
                    }
                }),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(Step::Statements(planned)))
}

fn change(key: &[u8], change: Change) -> Step {
    Step::Procedure(Procedure::Change {
        key: key.to_vec(),
        change,
    })
}

/// A key's time left in the form `EXPIRETIME` answers: an absolute instant.
pub(crate) fn expires_at(left_millis: i64) -> i64 {
    now_millis().saturating_add(left_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_takes_inclusive_bounds_counted_from_either_end() {
        let bytes = b"This is a string";
        assert_eq!(range(bytes, 0, 3), b"This");
        assert_eq!(range(bytes, -3, -1), b"ing");
        assert_eq!(range(bytes, 0, -1), bytes);
        assert_eq!(range(bytes, 10, 100), b"string");
        assert_eq!(range(bytes, 5, 2), b"");
        assert_eq!(range(b"", 0, -1), b"");
    }
}
