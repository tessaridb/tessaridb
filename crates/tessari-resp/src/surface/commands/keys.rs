//! The commands about keys: their existence, expiry, names and walks.

use tessaridb::Value;

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::arguments::{Expiry, expiry, expiry_value, integer};
use crate::surface::commands::strings::{expires_at, string_reply};
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned, value_of, values_of};
use crate::surface::values;

const NAMES: &[&str] = &[
    "DEL",
    "UNLINK",
    "EXISTS",
    "TOUCH",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "TYPE",
    "OBJECT",
    "DBSIZE",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "RENAME",
    "RENAMENX",
    "COPY",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// When an `EXPIRE … NX|XX|GT|LT` applies, judged against the expiry the key has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    /// Only a key with no expiry.
    NoExpiry,
    /// Only a key that has one.
    HasExpiry,
    /// Only when the new expiry is later.
    Later,
    /// Only when the new expiry is sooner.
    Sooner,
}

/// A time left as a count of `unit`: −2 no key, −1 no expiry.
fn left(value: &Value, millis: bool) -> Reply {
    match value {
        Value::Duration(span) => {
            let total = span
                .seconds()
                .saturating_mul(1_000)
                .saturating_add(i64::from(span.nanos() / 1_000_000));
            Reply::Integer(if millis {
                total
            } else {
                // Rounded to the nearest second, as the protocol's own does.
                total.saturating_add(500).div_euclid(1_000)
            })
        }
        Value::Null => Reply::Integer(-1),
        _ => Reply::Integer(-2),
    }
}

/// Plan `name` when it is a key command.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    let planned = match name {
        "DEL" | "UNLINK" => {
            if arguments.is_empty() {
                return Err(replies::arity(name));
            }
            let statements = arguments
                .iter()
                .map(|key| format!("DELETE {} RETURN BEFORE", binder.key(space, key)))
                .collect();
            Planned::many(
                statements,
                Box::new(|outcomes| {
                    let removed = outcomes
                        .into_iter()
                        .filter(|outcome| value_of(outcome.clone()) != Value::None)
                        .count();
                    Reply::Integer(i64::try_from(removed).unwrap_or(i64::MAX))
                }),
            )
        }
        "EXISTS" | "TOUCH" => {
            if arguments.is_empty() {
                return Err(replies::arity(name));
            }
            let reads: Vec<String> = arguments
                .iter()
                .map(|key| format!("TTL {}", binder.key(space, key)))
                .collect();
            Planned::one(
                format!("RETURN [{}]", reads.join(", ")),
                Box::new(|outcomes| {
                    let present = outcomes
                        .into_iter()
                        .next()
                        .map(values_of)
                        .unwrap_or_default()
                        .iter()
                        .filter(|left| **left != Value::None)
                        .count();
                    Reply::Integer(i64::try_from(present).unwrap_or(i64::MAX))
                }),
            )
        }
        "TTL" | "PTTL" | "EXPIRETIME" | "PEXPIRETIME" => {
            let [key] = arguments else {
                return Err(replies::arity(name));
            };
            let millis = name.starts_with('P');
            let absolute = name.ends_with("TIME");
            let target = binder.key(space, key);
            Planned::one(
                format!("RETURN TTL {target}"),
                Box::new(move |outcomes| {
                    let held = outcomes.into_iter().next().map_or(Value::None, value_of);
                    match left(&held, true) {
                        Reply::Integer(total) if total >= 0 && absolute => {
                            let at = expires_at(total);
                            Reply::Integer(if millis { at } else { at.div_euclid(1_000) })
                        }
                        Reply::Integer(total) if total >= 0 => left(&held, millis),
                        negative => negative,
                    }
                }),
            )
        }
        "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" => {
            let [key, count, options @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let at = expiry(integer(count)?, name.starts_with('P'), name.ends_with("AT"))
                .ok_or_else(|| {
                    Reply::error(format!(
                        "ERR invalid expire time in '{}' command",
                        name.to_ascii_lowercase()
                    ))
                })?;
            let when = match options {
                [] => None,
                [option] => Some(match option.to_ascii_uppercase().as_slice() {
                    b"NX" => When::NoExpiry,
                    b"XX" => When::HasExpiry,
                    b"GT" => When::Later,
                    b"LT" => When::Sooner,
                    _ => return Err(replies::syntax()),
                }),
                _ => return Err(replies::syntax()),
            };
            if let Some(when) = when {
                return Ok(Some(Step::Procedure(Procedure::ExpireWhen {
                    key: key.clone(),
                    at,
                    when,
                })));
            }
            expire(key, at, space, binder)
        }
        "PERSIST" => {
            let [key] = arguments else {
                return Err(replies::arity(name));
            };
            let target = binder.key(space, key);
            Planned::one(format!("PERSIST {target}"), Box::new(boolean))
        }
        "TYPE" => {
            let [key] = arguments else {
                return Err(replies::arity(name));
            };
            let target = binder.key(space, key);
            Planned::one(
                format!("GET {target}"),
                Box::new(|outcomes| {
                    let held = outcomes.into_iter().next().map_or(Value::None, value_of);
                    Reply::Status(
                        match string_reply(&held) {
                            Reply::Nil => "none",
                            Reply::Bulk(_) => "string",
                            _ => "unknown",
                        }
                        .into(),
                    )
                }),
            )
        }
        "OBJECT" => {
            let [subcommand, key] = arguments else {
                return Err(replies::arity(name));
            };
            let subcommand = subcommand.to_ascii_uppercase();
            if !matches!(
                subcommand.as_slice(),
                b"ENCODING" | b"REFCOUNT" | b"IDLETIME"
            ) {
                return Err(Reply::error(format!(
                    "ERR unknown subcommand '{}'. Try OBJECT HELP.",
                    String::from_utf8_lossy(&subcommand).to_ascii_lowercase()
                )));
            }
            let target = binder.key(space, key);
            Planned::one(
                format!("GET {target}"),
                Box::new(move |outcomes| {
                    let held = outcomes.into_iter().next().map_or(Value::None, value_of);
                    if held == Value::None {
                        return Reply::Nil;
                    }
                    match subcommand.as_slice() {
                        b"ENCODING" => match (&held, values::rendered(&held)) {
                            (Value::Number(tessaridb::Number::Integer(_)), _) => Reply::text("int"),
                            (_, Some(bytes)) if bytes.len() <= EMBEDDED => Reply::text("embstr"),
                            _ => Reply::text("raw"),
                        },
                        b"REFCOUNT" => Reply::Integer(1),
                        _ => Reply::Integer(0),
                    }
                }),
            )
        }
        "DBSIZE" => {
            if !arguments.is_empty() {
                return Err(replies::arity(name));
            }
            Planned::one(
                format!("SELECT count(*) AS n FROM {}", space.name),
                Box::new(|outcomes| {
                    let counted = outcomes.into_iter().next().map_or(Value::None, value_of);
                    match counted {
                        Value::Object(fields) => match fields.get("n") {
                            Some(Value::Number(tessaridb::Number::Integer(n))) => {
                                Reply::Integer(*n)
                            }
                            _ => Reply::Integer(0),
                        },
                        _ => Reply::Integer(0),
                    }
                }),
            )
        }
        "KEYS" => {
            let [pattern] = arguments else {
                return Err(replies::arity(name));
            };
            return Ok(Some(Step::Procedure(Procedure::Keys(pattern.clone()))));
        }
        "SCAN" => return scan(arguments).map(Some),
        "RANDOMKEY" => {
            if !arguments.is_empty() {
                return Err(replies::arity(name));
            }
            return Ok(Some(Step::Procedure(Procedure::RandomKey)));
        }
        "RENAME" | "RENAMENX" => {
            let [from, to] = arguments else {
                return Err(replies::arity(name));
            };
            return Ok(Some(Step::Procedure(Procedure::Rename {
                from: from.clone(),
                to: to.clone(),
                only_new: name == "RENAMENX",
            })));
        }
        "COPY" => {
            let [from, to, options @ ..] = arguments else {
                return Err(replies::arity(name));
            };
            let mut replace = false;
            let mut rest = options.iter();
            while let Some(option) = rest.next() {
                match option.to_ascii_uppercase().as_slice() {
                    b"REPLACE" => replace = true,
                    b"DB" => {
                        let index = rest.next().ok_or_else(replies::syntax)?;
                        if integer(index)? != 0 {
                            return Err(Reply::error("ERR DB index is out of range"));
                        }
                    }
                    _ => return Err(replies::syntax()),
                }
            }
            if from == to {
                return Err(Reply::error(
                    "ERR source and destination objects are the same",
                ));
            }
            return Ok(Some(Step::Procedure(Procedure::Copy {
                from: from.clone(),
                to: to.clone(),
                replace,
            })));
        }
        _ => return Ok(None),
    };
    Ok(Some(Step::Statements(planned)))
}

/// Strings up to this length the protocol's own server holds inline.
const EMBEDDED: usize = 44;

/// `EXPIRE key at`: 1 when the key was there, 0 when it was not.
pub(crate) fn expire(key: &[u8], at: Expiry, space: &Space, binder: &mut Binder) -> Planned {
    let target = binder.key(space, key);
    let bound = binder.bind(expiry_value(at));
    Planned::one(format!("EXPIRE {target} {bound}"), Box::new(boolean))
}

fn boolean(outcomes: Vec<tessaridb::Outcome>) -> Reply {
    let answered = outcomes.into_iter().next().map_or(Value::None, value_of);
    Reply::Integer(i64::from(answered == Value::Bool(true)))
}

fn scan(arguments: &[Vec<u8>]) -> Result<Step, Reply> {
    let [cursor, options @ ..] = arguments else {
        return Err(replies::arity("SCAN"));
    };
    let cursor = std::str::from_utf8(cursor)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| Reply::error("ERR invalid cursor"))?;
    let mut pattern = None;
    let mut count = 10_u64;
    let mut typed_away = false;
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        let value = rest.next().ok_or_else(replies::syntax)?;
        match option.to_ascii_uppercase().as_slice() {
            b"MATCH" => pattern = Some(value.clone()),
            b"COUNT" => {
                count = u64::try_from(integer(value)?)
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or_else(replies::syntax)?;
            }
            // Every key of a space is a string, so any other type is none.
            b"TYPE" => typed_away = !value.eq_ignore_ascii_case(b"string"),
            _ => return Err(replies::syntax()),
        }
    }
    if typed_away {
        return Ok(Step::Reply(Reply::Array(vec![
            Reply::text("0"),
            Reply::Array(Vec::new()),
        ])));
    }
    Ok(Step::Procedure(Procedure::Scan {
        cursor,
        pattern,
        count,
    }))
}

/// The longest common subsequence of two strings, or its length.
pub(crate) fn longest_common(first: &[u8], second: &[u8], only_length: bool) -> Reply {
    let width = second.len().saturating_add(1);
    let cells = first.len().saturating_add(1).saturating_mul(width);
    if cells > tessari_constants::RESP_LCS_MAX_CELLS {
        return Reply::error("ERR the two strings are too long to compare here");
    }
    let mut table = vec![0_u32; cells];
    for (row, a) in first.iter().enumerate() {
        for (column, b) in second.iter().enumerate() {
            let here = row
                .saturating_add(1)
                .saturating_mul(width)
                .saturating_add(column)
                .saturating_add(1);
            table[here] = if a == b {
                table[row.saturating_mul(width).saturating_add(column)].saturating_add(1)
            } else {
                table[here.saturating_sub(1)].max(table[here.saturating_sub(width)])
            };
        }
    }
    let total = table.last().copied().unwrap_or(0);
    if only_length {
        return Reply::Integer(i64::from(total));
    }
    let mut common = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    let (mut row, mut column) = (first.len(), second.len());
    while row > 0 && column > 0 {
        let here = row.saturating_mul(width).saturating_add(column);
        if first[row.saturating_sub(1)] == second[column.saturating_sub(1)] {
            common.push(first[row.saturating_sub(1)]);
            row = row.saturating_sub(1);
            column = column.saturating_sub(1);
        } else if table[here.saturating_sub(width)] >= table[here.saturating_sub(1)] {
            row = row.saturating_sub(1);
        } else {
            column = column.saturating_sub(1);
        }
    }
    common.reverse();
    Reply::Bulk(common)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_common_subsequence_and_its_length() {
        assert_eq!(
            longest_common(b"ohmytext", b"mynewtext", false),
            Reply::text("mytext")
        );
        assert_eq!(
            longest_common(b"ohmytext", b"mynewtext", true),
            Reply::Integer(6)
        );
        assert_eq!(longest_common(b"", b"abc", false), Reply::text(""));
        let long = vec![b'a'; 4096];
        assert!(matches!(
            longest_common(&long, &long, true),
            Reply::Error(_)
        ));
    }

    #[test]
    fn a_time_left_is_counted_as_the_protocol_counts_it() {
        let span = |millis: i64| {
            Value::Duration(
                tessaridb::Duration::new(
                    millis / 1000,
                    u32::try_from(millis % 1000).unwrap() * 1_000_000,
                )
                .unwrap(),
            )
        };
        assert_eq!(left(&span(1_499), false), Reply::Integer(1));
        assert_eq!(left(&span(1_500), false), Reply::Integer(2));
        assert_eq!(left(&span(1_500), true), Reply::Integer(1_500));
        assert_eq!(left(&Value::Null, false), Reply::Integer(-1));
        assert_eq!(left(&Value::None, true), Reply::Integer(-2));
    }
}
