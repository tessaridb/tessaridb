//! What a request and an answer hold.
//!
//! # Values travel in the store's own encoding
//!
//! Not JSON. The HTTP endpoint speaks JSON because a browser is owed JSON, and
//! it pays for that: fifteen value types projected onto six, with a decimal
//! quoted so it is not silently a double and a datetime, a duration and a record
//! reference all arriving as text. A client reading that back has to *guess* —
//! deciding `"12.34"` is a decimal and `"2s"` a duration by looking at them,
//! which is a parser inventing what a lossless encoder already knew.
//!
//! So a value here goes through `bgv_db_encoding::payload`, the same codec the
//! store writes records with. Fifteen types out, fifteen types back, nothing to
//! get wrong at either end.

use bgv_db::{AccessPath, Outcome, RecordId, Value};
use bgv_db_encoding::{decode_payload, encode_payload};

use crate::error::{Error, Result};
use crate::frame::{put_bytes, put_text, put_u32, take_bytes, take_text, take_u32};

/// A script to run, and who is running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The script.
    pub script: String,
    /// The credentials, when the caller has any.
    ///
    /// Optional because an open store runs anything, which is what makes an
    /// empty one usable; a closed store's refusal comes from the session rather
    /// than from a second rule here.
    pub credentials: Option<(String, String)>,
}

impl Request {
    /// The body of a request frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_text(&mut body, &self.script);
        match &self.credentials {
            Some((name, password)) => {
                body.push(1);
                put_text(&mut body, name);
                put_text(&mut body, password);
            }
            None => body.push(0),
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body does not hold what it claims.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (script, at) = take_text(body, 0)?;
        let flag = body.get(at).copied().ok_or(Error::Malformed)?;
        let credentials = match flag {
            0 => None,
            1 => {
                let (name, at) = take_text(body, at.saturating_add(1))?;
                let (password, _) = take_text(body, at)?;
                Some((name, password))
            }
            _ => return Err(Error::Malformed),
        };
        Ok(Self {
            script,
            credentials,
        })
    }
}

/// The tag an outcome carries, so a reader knows what follows.
mod tag {
    pub(super) const DONE: u8 = 0;
    pub(super) const RECORDS: u8 = 1;
    pub(super) const VALUE: u8 = 2;
    pub(super) const KEYS: u8 = 3;
    pub(super) const REMOVED: u8 = 4;
    /// An outcome shape this build does not know.
    ///
    /// `Outcome` is `#[non_exhaustive]`, so a newer store can answer with
    /// something this encoder has never seen. Saying so is honest; guessing at
    /// its content would not be.
    pub(super) const UNKNOWN: u8 = 255;
}

/// One statement's answer, on the wire.
#[must_use]
pub fn encode_outcome(outcome: &Outcome) -> Vec<u8> {
    let mut body = Vec::new();
    match outcome {
        Outcome::Done => body.push(tag::DONE),
        Outcome::Records { records, path } => {
            body.push(tag::RECORDS);
            body.push(path_tag(*path));
            put_u32(&mut body, u32::try_from(records.len()).unwrap_or(u32::MAX));
            for (id, value) in records {
                put_text(&mut body, &id.to_string());
                put_bytes(&mut body, encode_payload(value).as_slice());
            }
        }
        Outcome::Value(held) => {
            body.push(tag::VALUE);
            put_bytes(&mut body, encode_payload(held).as_slice());
        }
        Outcome::Keys(keys) => {
            body.push(tag::KEYS);
            put_u32(&mut body, u32::try_from(keys.len()).unwrap_or(u32::MAX));
            for key in keys {
                put_text(&mut body, &key.to_string());
            }
        }
        Outcome::Removed { count } => {
            body.push(tag::REMOVED);
            body.extend_from_slice(&count.to_be_bytes());
        }
        _ => body.push(tag::UNKNOWN),
    }
    body
}

/// What a client got back.
///
/// A separate type from [`Outcome`] rather than the same one, because a record
/// id on the wire is text: this protocol carries what the store answered, and a
/// client that wants to name a record writes it into its next script.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// The statement did its work.
    Done,
    /// Records, each with its identity as written and its value.
    Records {
        /// What was found.
        records: Vec<(String, Value)>,
        /// How, as the store names it.
        path: String,
    },
    /// One value.
    Value(Value),
    /// Keys, as written.
    Keys(Vec<String>),
    /// How many a conditional delete removed.
    Removed(u64),
    /// Something this build does not know how to read.
    Unknown,
}

/// Read one outcome back, and how much of the buffer it used.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the body does not hold what it claims.
pub fn decode_outcome(body: &[u8], at: usize) -> Result<(Answer, usize)> {
    let tag = body.get(at).copied().ok_or(Error::Malformed)?;
    let mut at = at.saturating_add(1);
    let answer = match tag {
        tag::DONE => Answer::Done,
        tag::RECORDS => {
            let path = path_name(body.get(at).copied().ok_or(Error::Malformed)?).to_owned();
            at = at.saturating_add(1);
            let (count, next) = take_u32(body, at)?;
            at = next;
            let mut records = Vec::new();
            for _ in 0..count {
                let (id, next) = take_text(body, at)?;
                let (bytes, next) = take_bytes(body, next)?;
                at = next;
                records.push((id, decode_payload(&bytes)?));
            }
            Answer::Records { records, path }
        }
        tag::VALUE => {
            let (bytes, next) = take_bytes(body, at)?;
            at = next;
            Answer::Value(decode_payload(&bytes)?)
        }
        tag::KEYS => {
            let (count, next) = take_u32(body, at)?;
            at = next;
            let mut keys = Vec::new();
            for _ in 0..count {
                let (key, next) = take_text(body, at)?;
                at = next;
                keys.push(key);
            }
            Answer::Keys(keys)
        }
        tag::REMOVED => {
            let end = at.checked_add(8).ok_or(Error::Malformed)?;
            let bytes = body.get(at..end).ok_or(Error::Malformed)?;
            let mut held = [0_u8; 8];
            held.copy_from_slice(bytes);
            at = end;
            Answer::Removed(u64::from_be_bytes(held))
        }
        _ => Answer::Unknown,
    };
    Ok((answer, at))
}

/// The access path, as one byte.
const fn path_tag(path: AccessPath) -> u8 {
    match path {
        AccessPath::Record => 0,
        AccessPath::Index => 1,
        AccessPath::Scan => 2,
    }
}

/// And back, as the name the store uses.
const fn path_name(tag: u8) -> &'static str {
    match tag {
        0 => "record",
        1 => "index",
        _ => "scan",
    }
}

/// The identity of a record, as text.
///
/// Kept as the store spells it rather than re-parsed, because a client that
/// wants to name a record writes that text into its next script — and a second
/// reading of a record id is a second place for the two to disagree.
#[must_use]
pub fn spell(id: &RecordId) -> String {
    id.to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db::{AccessPath, Outcome, RecordId, Value};

    use super::{Answer, Request, decode_outcome, encode_outcome};

    #[test]
    fn a_request_round_trips_with_and_without_credentials() {
        for credentials in [None, Some(("ada".to_owned(), "a long one".to_owned()))] {
            let held = Request {
                script: "SELECT * FROM users;".to_owned(),
                credentials,
            };
            let read = Request::decode(&held.encode()).expect("a request");
            assert_eq!(read, held);
        }
    }

    #[test]
    fn a_request_body_that_promises_more_than_it_holds_is_refused() {
        let held = Request {
            script: "SELECT * FROM users;".to_owned(),
            credentials: Some(("ada".to_owned(), "x".to_owned())),
        };
        let body = held.encode();
        for cut in 0..body.len() {
            assert!(
                Request::decode(&body[..cut]).is_err(),
                "a cut at {cut} parsed"
            );
        }
    }

    #[test]
    fn every_outcome_shape_round_trips() {
        let outcomes = [
            Outcome::Done,
            Outcome::Value(Value::from("held")),
            Outcome::Value(Value::None),
            Outcome::Keys(vec![RecordId::Int(1), RecordId::Text("a".to_owned())]),
            Outcome::Removed { count: 12_043 },
            Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                path: AccessPath::Index,
            },
        ];
        for outcome in &outcomes {
            let body = encode_outcome(outcome);
            let (answer, used) = decode_outcome(&body, 0).expect("an answer");
            assert_eq!(used, body.len(), "{outcome:?} left bytes unread");
            match (outcome, &answer) {
                (Outcome::Done, Answer::Done)
                | (Outcome::Value(_), Answer::Value(_))
                | (Outcome::Keys(_), Answer::Keys(_))
                | (Outcome::Removed { .. }, Answer::Removed(_))
                | (Outcome::Records { .. }, Answer::Records { .. }) => {}
                (held, read) => panic!("{held:?} came back as {read:?}"),
            }
        }
    }

    #[test]
    fn a_value_keeps_its_kind_where_json_would_have_flattened_it() {
        // The whole reason this protocol exists rather than the console reading
        // the HTTP endpoint: a decimal is a decimal and not a quoted string a
        // client has to decide about.
        let held = Outcome::Value(Value::Number(bgv_db::Number::Decimal(
            rust_decimal::Decimal::try_from(12.34_f64).expect("a decimal"),
        )));
        let (answer, _) = decode_outcome(&encode_outcome(&held), 0).expect("an answer");
        match answer {
            Answer::Value(Value::Number(bgv_db::Number::Decimal(read))) => {
                assert_eq!(read.to_string(), "12.34");
            }
            other => panic!("a decimal came back as {other:?}"),
        }
    }

    #[test]
    fn several_outcomes_read_back_in_order_from_one_body() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_outcome(&Outcome::Done));
        body.extend_from_slice(&encode_outcome(&Outcome::Removed { count: 3 }));
        body.extend_from_slice(&encode_outcome(&Outcome::Value(Value::from("last"))));

        let (first, at) = decode_outcome(&body, 0).expect("first");
        let (second, at) = decode_outcome(&body, at).expect("second");
        let (third, at) = decode_outcome(&body, at).expect("third");
        assert_eq!(first, Answer::Done);
        assert_eq!(second, Answer::Removed(3));
        assert_eq!(third, Answer::Value(Value::from("last")));
        assert_eq!(at, body.len());
    }
}
