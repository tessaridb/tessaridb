//! The hash-field expiry commands — `HEXPIRE`, `HPEXPIRE`, `HEXPIREAT`,
//! `HPEXPIREAT`, `HTTL`, `HPTTL`, `HEXPIRETIME`, `HPEXPIRETIME`, `HPERSIST` —
//! each one `HASH EXPIRE`, `HASH TTL` or `HASH PERSIST` statement (ADR-0132).

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::{expiry, expiry_value, integer};
use crate::surface::commands::containers::{first, text};
use crate::surface::commands::keys::left;
use crate::surface::commands::strings::expires_at;
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};

const NAMES: &[&str] = &[
    "HEXPIRE",
    "HPEXPIRE",
    "HEXPIREAT",
    "HPEXPIREAT",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    "HPERSIST",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The fields of `FIELDS numfields field …`, bound and joined.
fn fields(block: &[Vec<u8>], binder: &mut Binder) -> Result<String, Reply> {
    let [word, count, named @ ..] = block else {
        return Err(missing_fields());
    };
    if !word.eq_ignore_ascii_case(b"FIELDS") {
        return Err(missing_fields());
    }
    let count = integer(count)?;
    if count <= 0 {
        return Err(Reply::error(
            "ERR Parameter `numFields` should be greater than 0",
        ));
    }
    if usize::try_from(count).ok() != Some(named.len()) {
        return Err(Reply::error(
            "ERR The `numfields` parameter must match the number of arguments",
        ));
    }
    Ok(named
        .iter()
        .map(|field| Ok(binder.bind(Value::String(text(field)?))))
        .collect::<Result<Vec<_>, Reply>>()?
        .join(", "))
}

fn missing_fields() -> Reply {
    Reply::error("ERR Mandatory argument FIELDS is missing or not at the right position")
}

/// Each element of an answered array, mapped.
fn each(outcomes: Vec<tessaridb::Outcome>, answer: impl Fn(&Value) -> Reply) -> Reply {
    match first(outcomes) {
        Value::Array(items) => Reply::Array(items.iter().map(answer).collect()),
        _ => Reply::Array(Vec::new()),
    }
}

fn code(value: &Value) -> Reply {
    match value {
        Value::Number(Number::Integer(code)) => Reply::Integer(*code),
        _ => Reply::Integer(-2),
    }
}

/// Plan `name` when it is one of these commands.
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
        "HEXPIRE" | "HPEXPIRE" | "HEXPIREAT" | "HPEXPIREAT" => {
            let [count, rest @ ..] = rest else {
                return Err(replies::arity(name));
            };
            let count = integer(count)?;
            if count < 0 {
                return Err(Reply::error("ERR invalid expire time, must be >= 0"));
            }
            let when =
                expiry(count, name.starts_with("HP"), name.ends_with("AT")).ok_or_else(|| {
                    Reply::error(format!(
                        "ERR invalid expire time in '{}' command",
                        name.to_ascii_lowercase()
                    ))
                })?;
            let (condition, block) = match rest.split_first() {
                Some((word, block)) if !word.eq_ignore_ascii_case(b"FIELDS") => {
                    let condition = match word.to_ascii_uppercase().as_slice() {
                        b"NX" => " IF NONE",
                        b"XX" => " IF SOME",
                        b"GT" => " IF LATER",
                        b"LT" => " IF SOONER",
                        _ => return Err(missing_fields()),
                    };
                    (condition, block)
                }
                _ => ("", rest),
            };
            let fields = fields(block, binder)?;
            let when = binder.bind(expiry_value(when));
            Planned::one(
                format!("HASH EXPIRE {target} {fields} AT {when}{condition}"),
                Box::new(|outcomes| each(outcomes, code)),
            )
        }
        "HPERSIST" => {
            let fields = fields(rest, binder)?;
            Planned::one(
                format!("HASH PERSIST {target} {fields}"),
                Box::new(|outcomes| each(outcomes, code)),
            )
        }
        _ => {
            let fields = fields(rest, binder)?;
            let millis = name.starts_with("HP");
            let absolute = name.ends_with("TIME");
            Planned::one(
                format!("HASH TTL {target} {fields}"),
                Box::new(move |outcomes| {
                    each(outcomes, |held| match left(held, true) {
                        Reply::Integer(total) if total >= 0 && absolute => {
                            let at = expires_at(total);
                            Reply::Integer(if millis { at } else { at.div_euclid(1_000) })
                        }
                        Reply::Integer(total) if total >= 0 => left(held, millis),
                        negative => negative,
                    })
                }),
            )
        }
    };
    Ok(Some(Step::Statements(planned)))
}
