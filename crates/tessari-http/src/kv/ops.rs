//! The key-value operations: what each reads from the request, the statement it
//! runs, and what it answers (ADR-0090 §1).

use axum::http::Method;
use tessaridb::{Db, Outcome, Parameters, Session, Value};

use super::{Aimed, positive_duration};
use crate::json;
use crate::respond::{Answer, failure};

/// The most keys one listing answers, and how many it answers unasked.
const LIST_MOST: u32 = 1000;
const LIST_DEFAULT: u32 = 100;

/// A condition a write may carry.
#[derive(Clone, Copy)]
pub(crate) enum Condition {
    Absent,
    Present,
}

/// One key-value request, read and checked before any session is opened.
pub(crate) enum Request {
    Read {
        key: String,
    },
    Write {
        key: String,
        value: Value,
        condition: Option<Condition>,
        expire: Option<Value>,
    },
    Delete {
        key: String,
    },
    Swap {
        key: String,
        expect: Value,
        value: Value,
        expire: Option<Value>,
    },
    Incr {
        key: String,
        by: Value,
    },
    Expire {
        key: String,
        expire: Value,
    },
    Persist {
        key: String,
    },
    Lock {
        key: String,
        holder: Value,
        expire: Value,
    },
    Unlock {
        key: String,
        holder: Value,
    },
    List {
        prefix: Option<Value>,
        after: Option<Value>,
        limit: u32,
    },
}

type Refused = Answer;

impl Request {
    /// Read what `method` and `aimed` ask for, or the `400`/`404`/`405` that says
    /// why it cannot be asked.
    pub(crate) fn read(method: &Method, aimed: &Aimed<'_>, body: &str) -> Result<Self, Refused> {
        let Some(op) = aimed.op else {
            return if *method == Method::GET {
                list(aimed)
            } else {
                Err(wrong_method())
            };
        };
        let allowed = match op {
            "key" => [Method::GET, Method::PUT, Method::DELETE].contains(method),
            "swap" | "incr" | "expire" | "persist" | "lock" | "unlock" => *method == Method::POST,
            _ => {
                return Err(Answer::new(
                    404,
                    r#"{"error":"no such key-value operation; see the route list in the documentation"}"#
                        .to_owned(),
                ));
            }
        };
        if !allowed {
            return Err(wrong_method());
        }
        let key = match &aimed.key {
            Some(key) if !key.is_empty() => key.clone(),
            _ => return Err(Answer::bad_request("name a key after the operation")),
        };
        let expire = || optional_duration(aimed);
        Ok(match (op, method.as_str()) {
            ("key", "GET") => Self::Read { key },
            ("key", "DELETE") => Self::Delete { key },
            ("key", _) => Self::Write {
                key,
                value: one_value(body)?,
                condition: match aimed.param("if").as_deref() {
                    None => None,
                    Some("absent") => Some(Condition::Absent),
                    Some("present") => Some(Condition::Present),
                    Some(_) => return Err(Answer::bad_request("`if` is `absent` or `present`")),
                },
                expire: expire()?,
            },
            ("swap", _) => {
                let (expect, value) = swap_body(body)?;
                Self::Swap {
                    key,
                    expect,
                    value,
                    expire: expire()?,
                }
            }
            ("incr", _) => Self::Incr {
                key,
                by: match aimed.param("by") {
                    None => Value::from(1_i64),
                    Some(text) => match text.parse::<i64>() {
                        Ok(by) => Value::from(by),
                        Err(_) => return Err(Answer::bad_request("`by` is a whole number")),
                    },
                },
            },
            ("expire", _) => Self::Expire {
                key,
                expire: required_duration(aimed)?,
            },
            ("persist", _) => Self::Persist { key },
            ("lock", _) => Self::Lock {
                key,
                holder: holder(aimed)?,
                expire: required_duration(aimed)?,
            },
            _ => Self::Unlock {
                key,
                holder: holder(aimed)?,
            },
        })
    }

    /// Run the statement this request stands for, in `session`, against `space`.
    pub(crate) fn run(self, session: &mut Session<'_>, db: &Db, space: &str) -> Answer {
        let mut given = Parameters::new();
        let script = match &self {
            Self::Read { key } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                format!("GET {space}:$k; RETURN TTL {space}:$k;")
            }
            Self::Write {
                key,
                value,
                condition,
                expire,
            } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("v".to_owned(), value.clone());
                let condition = match condition {
                    None => "",
                    Some(Condition::Absent) => " IF ABSENT",
                    Some(Condition::Present) => " IF PRESENT",
                };
                format!(
                    "SET {space}:$k = $v{condition}{};",
                    expiring(&mut given, expire.as_ref())
                )
            }
            Self::Delete { key } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                format!("DELETE {space}:$k RETURN BEFORE;")
            }
            Self::Swap {
                key,
                expect,
                value,
                expire,
            } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("e".to_owned(), expect.clone());
                given.insert("v".to_owned(), value.clone());
                format!(
                    "SET {space}:$k = $v IF = $e{};",
                    expiring(&mut given, expire.as_ref())
                )
            }
            Self::Incr { key, by } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("n".to_owned(), by.clone());
                format!("INCR {space}:$k BY $n;")
            }
            Self::Expire { key, expire } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("t".to_owned(), expire.clone());
                format!("EXPIRE {space}:$k $t;")
            }
            Self::Persist { key } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                format!("PERSIST {space}:$k;")
            }
            // Taken if nobody holds it, extended if this holder does — two
            // conditional writes, never an unconditional one.
            Self::Lock {
                key,
                holder,
                expire,
            } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("h".to_owned(), holder.clone());
                given.insert("t".to_owned(), expire.clone());
                format!(
                    "SET {space}:$k = $h IF ABSENT EXPIRE $t; SET {space}:$k = $h IF = $h EXPIRE $t;"
                )
            }
            // Never a delete, and never a write without an expiry: a delete after
            // the lease lapsed removes the next holder's lock, and a hand-back
            // with no expiry makes the key permanent (ref: G035, measured).
            Self::Unlock { key, holder } => {
                given.insert("k".to_owned(), Value::String(key.clone()));
                given.insert("h".to_owned(), holder.clone());
                format!("SET {space}:$k = 'free' IF = $h EXPIRE 1ms;")
            }
            Self::List {
                prefix,
                after,
                limit,
            } => {
                let mut script = format!("KEYS FROM {space}");
                if let Some(prefix) = prefix {
                    given.insert("p".to_owned(), prefix.clone());
                    script.push_str(" PREFIX $p");
                }
                if let Some(after) = after {
                    given.insert("a".to_owned(), after.clone());
                    script.push_str(" AFTER $a");
                }
                format!("{script} LIMIT {limit};")
            }
        };
        let outcomes = match session.run_with(&script, &given) {
            Ok(outcomes) => outcomes,
            Err(error) => return failure(&error),
        };
        self.answer(db, &outcomes)
    }

    /// What the statement's outcomes say, in this route's words.
    fn answer(&self, db: &Db, outcomes: &[Outcome]) -> Answer {
        let values: Vec<&Value> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                Outcome::Value(held) => Some(held),
                _ => None,
            })
            .collect();
        let first = values.first().copied();
        let flag = |name: &str, held: bool| Answer::new(200, format!(r#"{{"{name}":{held}}}"#));
        match self {
            Self::Read { .. } => match first {
                None | Some(Value::None) => {
                    Answer::new(404, r#"{"error":"no such key"}"#.to_owned())
                }
                Some(value) => {
                    let names = db
                        .names_in(&[(tessaridb::RecordId::Int(0), value.clone())])
                        .unwrap_or_default();
                    let mut body = String::from(r#"{"value":"#);
                    json::write(&mut body, value, &names);
                    body.push_str(r#","ttl":"#);
                    match values.get(1) {
                        Some(Value::Duration(left)) => json::string(&mut body, &left.to_literal()),
                        _ => body.push_str("null"),
                    }
                    body.push('}');
                    Answer::new(200, body)
                }
            },
            Self::Write { .. } | Self::Swap { .. } => {
                // A plain write answers `ok` and no value; it always wrote.
                flag(
                    "written",
                    first.is_none_or(|held| matches!(held, Value::Bool(true))),
                )
            }
            Self::Delete { .. } => flag("deleted", !matches!(first, None | Some(Value::None))),
            Self::Incr { .. } => {
                let mut body = String::from(r#"{"value":"#);
                json::write(
                    &mut body,
                    first.unwrap_or(&Value::Null),
                    &json::Names::new(),
                );
                body.push('}');
                Answer::new(200, body)
            }
            Self::Expire { .. } | Self::Persist { .. } => {
                flag("found", matches!(first, Some(Value::Bool(true))))
            }
            Self::Lock { .. } => flag(
                "held",
                values.iter().any(|held| matches!(held, Value::Bool(true))),
            ),
            Self::Unlock { .. } => flag("released", matches!(first, Some(Value::Bool(true)))),
            Self::List { .. } => {
                let mut body = String::from(r#"{"keys":["#);
                let keys = outcomes.iter().find_map(|outcome| match outcome {
                    Outcome::Keys(keys) => Some(keys),
                    _ => None,
                });
                for (position, key) in keys.into_iter().flatten().enumerate() {
                    if position > 0 {
                        body.push(',');
                    }
                    json::string(&mut body, &key.to_string());
                }
                body.push_str("]}");
                Answer::new(200, body)
            }
        }
    }
}

/// ` EXPIRE $t` with the duration bound, or nothing.
fn expiring(given: &mut Parameters, expire: Option<&Value>) -> &'static str {
    match expire {
        Some(held) => {
            given.insert("t".to_owned(), held.clone());
            " EXPIRE $t"
        }
        None => "",
    }
}

fn list(aimed: &Aimed<'_>) -> Result<Request, Refused> {
    let limit = match aimed.param("limit") {
        None => LIST_DEFAULT,
        Some(text) => match text.parse::<u32>() {
            Ok(limit) if (1..=LIST_MOST).contains(&limit) => limit,
            _ => {
                return Err(Answer::bad_request(&format!(
                    "`limit` is a whole number from 1 to {LIST_MOST}"
                )));
            }
        },
    };
    Ok(Request::List {
        prefix: aimed
            .param("prefix")
            .filter(|held| !held.is_empty())
            .map(Value::String),
        after: aimed.param("after").map(Value::String),
        limit,
    })
}

fn optional_duration(aimed: &Aimed<'_>) -> Result<Option<Value>, Refused> {
    aimed
        .param("expire")
        .map(|text| positive_duration(&text).ok_or_else(bad_duration))
        .transpose()
}

fn required_duration(aimed: &Aimed<'_>) -> Result<Value, Refused> {
    optional_duration(aimed)?.ok_or_else(bad_duration)
}

fn bad_duration() -> Refused {
    Answer::bad_request("`expire` is a positive duration such as `30s` or `1h30m`")
}

fn holder(aimed: &Aimed<'_>) -> Result<Value, Refused> {
    match aimed.param("holder") {
        Some(holder) if !holder.is_empty() => Ok(Value::String(holder)),
        _ => Err(Answer::bad_request("a lock names its `holder`")),
    }
}

/// The body as one TessariQL value, read in isolation — `/series`' rule.
fn one_value(body: &str) -> Result<Value, Refused> {
    tessaridb::value_of(body.trim())
        .map_err(|_| Answer::bad_request("the body is one TessariQL value, written as a literal"))
}

fn swap_body(body: &str) -> Result<(Value, Value), Refused> {
    let refused = || Answer::bad_request("the body is `{ expect: <value>, value: <value> }`");
    let Value::Object(fields) = one_value(body)? else {
        return Err(refused());
    };
    match (fields.get("expect"), fields.get("value")) {
        (Some(expect), Some(value)) if fields.len() == 2 => Ok((expect.clone(), value.clone())),
        _ => Err(refused()),
    }
}

fn wrong_method() -> Refused {
    Answer::new(
        405,
        r#"{"error":"that route takes another method"}"#.to_owned(),
    )
}
