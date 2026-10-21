//! Reading arguments, and the commands about the connection rather than the
//! store.

use std::time::{SystemTime, UNIX_EPOCH};

use tessaridb::{BUILD_VERSION, Datetime, Duration, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;

/// Every command this node answers, as `COMMAND LIST` names them.
pub(crate) const SERVED: &[&str] = &[
    "append",
    "auth",
    "client",
    "command",
    "copy",
    "dbsize",
    "decr",
    "decrby",
    "del",
    "discard",
    "echo",
    "exec",
    "exists",
    "expire",
    "expireat",
    "expiretime",
    "get",
    "getdel",
    "getex",
    "getrange",
    "getset",
    "hello",
    "incr",
    "incrby",
    "incrbyfloat",
    "info",
    "keys",
    "lcs",
    "mget",
    "mset",
    "msetnx",
    "multi",
    "object",
    "persist",
    "pexpire",
    "pexpireat",
    "pexpiretime",
    "ping",
    "psetex",
    "pttl",
    "quit",
    "randomkey",
    "rename",
    "renamenx",
    "reset",
    "scan",
    "select",
    "set",
    "setex",
    "setnx",
    "setrange",
    "strlen",
    "substr",
    "time",
    "touch",
    "ttl",
    "type",
    "unlink",
    "tessaridb.use",
];

/// An expiry a command states: a span from now, or an instant.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Expiry {
    In(Duration),
    At(Datetime),
}

/// The value an `EXPIRE` clause binds.
pub(crate) fn expiry_value(expiry: Expiry) -> Value {
    match expiry {
        Expiry::In(span) => Value::Duration(span),
        Expiry::At(instant) => Value::Datetime(instant),
    }
}

/// `bytes` as a signed integer, or the reply that says it is not one.
pub(crate) fn integer(bytes: &[u8]) -> Result<i64, Reply> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(replies::not_an_integer)
}

/// An expiry from a count of `unit`s, relative or absolute; `None` when the
/// count is past what a span or an instant holds.
pub(crate) fn expiry(count: i64, millis: bool, absolute: bool) -> Option<Expiry> {
    let (seconds, nanos) = if millis {
        let nanos = u32::try_from(count.rem_euclid(1_000))
            .ok()?
            .checked_mul(1_000_000)?;
        (count.div_euclid(1_000), nanos)
    } else {
        (count, 0)
    };
    Some(if absolute {
        Expiry::At(Datetime::new(seconds, nanos)?)
    } else {
        Expiry::In(Duration::new(seconds, nanos)?)
    })
}

/// Milliseconds since the Unix epoch, by this node's clock.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

fn word(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes.to_ascii_uppercase()).into_owned()
}

/// The commands about the connection, or `None` for a command about the store.
pub(crate) fn connection(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    let step = match name {
        "PING" => match arguments {
            [] => Step::Reply(Reply::Status("PONG".into())),
            [message] => Step::Reply(Reply::Bulk(message.clone())),
            _ => return Err(replies::arity(name)),
        },
        "ECHO" => match arguments {
            [message] => Step::Reply(Reply::Bulk(message.clone())),
            _ => return Err(replies::arity(name)),
        },
        "QUIT" => Step::Procedure(Procedure::Quit),
        "RESET" => Step::Procedure(Procedure::Reset),
        "HELLO" => hello(arguments)?,
        "AUTH" => match arguments {
            [password] => Step::Procedure(Procedure::Auth {
                user: None,
                password: password.clone(),
            }),
            [user, password] => Step::Procedure(Procedure::Auth {
                user: Some(user.clone()),
                password: password.clone(),
            }),
            _ => return Err(replies::arity(name)),
        },
        "CLIENT" => {
            if arguments.is_empty() {
                return Err(replies::arity(name));
            }
            Step::Procedure(Procedure::Client(arguments.to_vec()))
        }
        "SELECT" => match arguments {
            [index] => {
                if integer(index)? == 0 {
                    Step::Reply(Reply::OK)
                } else {
                    return Err(Reply::error("ERR DB index is out of range"));
                }
            }
            _ => return Err(replies::arity(name)),
        },
        "COMMAND" => Step::Reply(command(arguments)),
        "INFO" => Step::Reply(Reply::Bulk(
            format!(
                "# Server\r\nserver_name:tessaridb\r\ntessaridb_version:{BUILD_VERSION}\r\n\
                 resp_version:3\r\n"
            )
            .into_bytes(),
        )),
        "TIME" => {
            let millis = now_millis();
            let micros = millis.rem_euclid(1_000).saturating_mul(1_000);
            Step::Reply(Reply::Array(vec![
                Reply::text(millis.div_euclid(1_000).to_string()),
                Reply::text(micros.to_string()),
            ]))
        }
        "MULTI" => Step::Procedure(Procedure::Multi),
        "EXEC" => Step::Procedure(Procedure::Exec),
        "DISCARD" => Step::Procedure(Procedure::Discard),
        "TESSARIDB.USE" => match arguments {
            [namespace, database, space] => {
                let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
                let space = Space::of(&text(namespace), &text(database), &text(space)).ok_or_else(
                    || Reply::error("ERR a namespace, a database and a space are identifiers"),
                )?;
                Step::Procedure(Procedure::Use(space))
            }
            _ => return Err(replies::arity(name)),
        },
        _ => return Ok(None),
    };
    Ok(Some(step))
}

fn hello(arguments: &[Vec<u8>]) -> Result<Step, Reply> {
    let mut protocol = None;
    let mut auth = None;
    let mut name = None;
    let mut rest = arguments.iter();
    if let Some(version) = rest.next() {
        protocol =
            Some(integer(version).map_err(|_| {
                Reply::error("ERR Protocol version is not an integer or out of range")
            })?);
    }
    while let Some(option) = rest.next() {
        match word(option).as_str() {
            "AUTH" => {
                let (Some(user), Some(password)) = (rest.next(), rest.next()) else {
                    return Err(replies::syntax());
                };
                auth = Some((user.clone(), password.clone()));
            }
            "SETNAME" => {
                let Some(given) = rest.next() else {
                    return Err(replies::syntax());
                };
                name = Some(given.clone());
            }
            _ => return Err(replies::syntax()),
        }
    }
    Ok(Step::Procedure(Procedure::Hello {
        protocol,
        auth,
        name,
    }))
}

fn command(arguments: &[Vec<u8>]) -> Reply {
    let names = || {
        SERVED
            .iter()
            .map(|name| Reply::text(*name))
            .collect::<Vec<_>>()
    };
    match arguments.first().map(|first| word(first)).as_deref() {
        Some("COUNT") => Reply::Integer(i64::try_from(SERVED.len()).unwrap_or(i64::MAX)),
        Some("LIST") => Reply::Array(names()),
        Some("DOCS") => Reply::Map(Vec::new()),
        Some("INFO") => Reply::Array(arguments[1..].iter().map(|_| Reply::Nil).collect()),
        _ => Reply::Array(Vec::new()),
    }
}
