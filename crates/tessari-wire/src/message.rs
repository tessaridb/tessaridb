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
//! So a value here goes through `tessari_encoding::payload`, the same codec the
//! store writes records with. Fifteen types out, fifteen types back, nothing to
//! get wrong at either end.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_ql::Parameters;
// `AccessPath` and `Outcome` are what the node encodes *from*; a client only
// ever decodes into `Answer`, so neither name reaches the client half.
#[cfg(feature = "server")]
use tessari_session::{AccessPath, Outcome};
use tessari_types::{RecordId, TableId, Value};

use crate::error::{Error, Result};
use crate::frame::{put_bytes, put_text, put_u32, take_bytes, take_text, take_u32};

/// What the tables an answer's references point at are called.
///
/// A record reference holds a **table id**, and the name the language writes
/// lives in the catalog — which is on the server. A client cannot resolve one
/// and has nowhere to look, so a reference would render as `<record 3:7>`: the
/// one thing a console that promises its output pastes back cannot print. The
/// server already computes exactly this map for its own rendering, including the
/// short-circuit that skips the catalog entirely when an answer holds no
/// reference at all.
pub type Names = BTreeMap<TableId, String>;

/// The names an outcome's references need, resolved against the catalog.
///
/// Public and shared rather than written once in the node and once in whatever
/// renders an embedded answer: "which answers can hold a reference" is one rule,
/// and two copies of it drift the first time a third shape gains a value.
///
/// `Db::names_in` walks the values before it opens anything, so an answer
/// holding no reference — which is most of them — costs a walk and no
/// transaction.
///
/// Server-side: it takes a `Db`, which is the catalog it resolves against, and
/// a client has neither.
#[cfg(feature = "server")]
#[must_use]
pub fn names_for(db: &tessaridb::Db, outcome: &Outcome) -> Names {
    match outcome {
        Outcome::Records { records, .. } => db.names_in(records).unwrap_or_default(),
        // Wrapped so the same walk finds it. The identity is a placeholder: the
        // walk reads values and never looks at what they are keyed by.
        Outcome::Value(held) => db
            .names_in(&[(RecordId::Int(0), held.clone())])
            .unwrap_or_default(),
        // Keys are record identities and hold no values, and the rest hold
        // nothing at all.
        _ => Names::new(),
    }
}

/// A script to run, and who is running it.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// The script.
    pub script: String,
    /// The credentials, when the caller has any.
    ///
    /// Optional because an open store runs anything, which is what makes an
    /// empty one usable; a closed store's refusal comes from the session rather
    /// than from a second rule here.
    pub credentials: Option<(String, String)>,
    /// The values the script's parameters are bound to.
    ///
    /// Carried in the store's own codec rather than as text the server parses.
    /// A value the server has to *read* is a value that can be read as something
    /// else, which is the thing binding after parsing exists to make impossible
    /// — undoing it at the wire would be a strange place to give it back.
    pub parameters: Parameters,
}

/// Written by hand rather than derived, because the derived one prints the
/// password.
///
/// Nothing in this build prints a request — but a struct holding a credential
/// and answering `{:?}` with it is how a password reaches a log line, and the
/// line that does it is always somewhere else and written later. The name is
/// shown because knowing *who* a refused request claimed to be is the whole
/// value of printing one; the secret is not.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("script", &self.script)
            .field(
                "as",
                &self
                    .credentials
                    .as_ref()
                    .map_or("nobody", |(name, _)| name.as_str()),
            )
            .field("parameters", &self.parameters.keys())
            .finish_non_exhaustive()
    }
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
        put_u32(
            &mut body,
            u32::try_from(self.parameters.len()).unwrap_or(u32::MAX),
        );
        for (name, value) in &self.parameters {
            put_text(&mut body, name);
            put_bytes(&mut body, encode_payload(value).as_slice());
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
        // The offset travels out of the match rather than being recomputed from
        // the lengths afterwards: recomputing it would mean this reader knowing
        // how the writer frames a string, which is exactly the coupling a pair
        // of `put`/`take` helpers exists to remove.
        let (credentials, at) = match flag {
            0 => (None, at.saturating_add(1)),
            1 => {
                let (name, at) = take_text(body, at.saturating_add(1))?;
                let (password, at) = take_text(body, at)?;
                (Some((name, password)), at)
            }
            _ => return Err(Error::Malformed),
        };
        let (count, mut at) = take_u32(body, at)?;
        let mut parameters = Parameters::new();
        for _ in 0..count {
            let (name, next) = take_text(body, at)?;
            let (bytes, next) = take_bytes(body, next)?;
            parameters.insert(name, decode_payload(&bytes)?);
            at = next;
        }
        Ok(Self {
            script,
            credentials,
            parameters,
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
    #[cfg(feature = "server")]
    pub(super) const UNKNOWN: u8 = 255;
}

/// One statement's answer, on the wire.
///
/// `names` covers the references this outcome carries and nothing else; for
/// every other outcome shape it is unread, because none of them can hold one.
#[cfg(feature = "server")]
#[must_use]
pub fn encode_outcome(outcome: &Outcome, names: &Names) -> Vec<u8> {
    let mut body = Vec::new();
    match outcome {
        Outcome::Done => body.push(tag::DONE),
        Outcome::Records { records, path } => {
            body.push(tag::RECORDS);
            body.push(path_tag(*path));
            put_names(&mut body, names);
            put_u32(&mut body, u32::try_from(records.len()).unwrap_or(u32::MAX));
            for (id, value) in records {
                put_text(&mut body, &id.to_string());
                put_bytes(&mut body, encode_payload(value).as_slice());
            }
        }
        Outcome::Value(held) => {
            body.push(tag::VALUE);
            put_names(&mut body, names);
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
        /// What the tables these records reference are called.
        names: Names,
    },
    /// One value, and the names of the tables it references.
    Value {
        /// What was answered.
        value: Value,
        /// What the tables it references are called.
        names: Names,
    },
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
            let (names, next) = take_names(body, at)?;
            at = next;
            let (count, next) = take_u32(body, at)?;
            at = next;
            let mut records = Vec::new();
            for _ in 0..count {
                let (id, next) = take_text(body, at)?;
                let (bytes, next) = take_bytes(body, next)?;
                at = next;
                records.push((id, decode_payload(&bytes)?));
            }
            Answer::Records {
                records,
                path,
                names,
            }
        }
        tag::VALUE => {
            let (names, next) = take_names(body, at)?;
            let (bytes, next) = take_bytes(body, next)?;
            at = next;
            Answer::Value {
                value: decode_payload(&bytes)?,
                names,
            }
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

/// The names, as a count and that many pairs.
#[cfg(feature = "server")]
fn put_names(body: &mut Vec<u8>, names: &Names) {
    put_u32(body, u32::try_from(names.len()).unwrap_or(u32::MAX));
    for (table, name) in names {
        put_u32(body, table.get());
        put_text(body, name);
    }
}

/// And back.
fn take_names(body: &[u8], at: usize) -> Result<(Names, usize)> {
    let (count, mut at) = take_u32(body, at)?;
    let mut names = Names::new();
    for _ in 0..count {
        let (table, next) = take_u32(body, at)?;
        let (name, next) = take_text(body, next)?;
        at = next;
        names.insert(TableId::new(table), name);
    }
    Ok((names, at))
}

/// The access path, as one byte.
#[cfg(feature = "server")]
const fn path_tag(path: AccessPath) -> u8 {
    match path {
        AccessPath::Record => 0,
        AccessPath::Index => 1,
        AccessPath::Scan => 2,
        AccessPath::Ordered => 3,
    }
}

/// And back, as the name the store uses.
///
/// An unknown tag reads as the scan, which is the honest answer to a path this
/// build has no name for: it is the one path that promises nothing.
const fn path_name(tag: u8) -> &'static str {
    match tag {
        0 => "record",
        1 => "index",
        3 => "ordered",
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

// The node's encoding half is what these exercise — `encode_outcome`, the
// access-path tag, `named` — so they belong to the same feature it does.
#[cfg(all(test, feature = "server"))]
mod tests {
    #![allow(clippy::panic)]

    use tessari_session::{AccessPath, Outcome, Parameters};
    use tessari_types::{Number, RecordId, RecordRef, TableId, Value};

    use super::{Answer, Names, Request, decode_outcome, encode_outcome};

    /// No answer below carries a reference, so none of them needs a name.
    fn unnamed() -> Names {
        Names::new()
    }

    #[test]
    fn a_request_round_trips_with_and_without_credentials() {
        for credentials in [None, Some(("ada".to_owned(), "a long one".to_owned()))] {
            let held = Request {
                script: "SELECT * FROM users;".to_owned(),
                credentials,
                parameters: Parameters::new(),
            };
            let read = Request::decode(&held.encode()).expect("a request");
            assert_eq!(read, held);
        }
    }

    #[test]
    fn a_request_does_not_print_the_password_it_carries() {
        // Nothing prints one today. The line that does is always somewhere else
        // and written later, which is exactly why this is asserted here.
        let held = Request {
            script: "SELECT * FROM users;".to_owned(),
            credentials: Some(("ada".to_owned(), "a long one".to_owned())),
            parameters: Parameters::new(),
        };
        let printed = format!("{held:?}");
        assert!(!printed.contains("a long one"), "{printed}");
        assert!(printed.contains("ada"), "{printed}");
    }

    #[test]
    fn a_request_body_that_promises_more_than_it_holds_is_refused() {
        let mut parameters = Parameters::new();
        parameters.insert("who".to_owned(), Value::String("ada".to_owned()));
        let held = Request {
            script: "SELECT * FROM users WHERE name = $who;".to_owned(),
            credentials: Some(("ada".to_owned(), "x".to_owned())),
            parameters,
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
            let body = encode_outcome(outcome, &unnamed());
            let (answer, used) = decode_outcome(&body, 0).expect("an answer");
            assert_eq!(used, body.len(), "{outcome:?} left bytes unread");
            match (outcome, &answer) {
                (Outcome::Done, Answer::Done)
                | (Outcome::Value(_), Answer::Value { .. })
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
        let held = Outcome::Value(Value::Number(Number::Decimal(
            rust_decimal::Decimal::try_from(12.34_f64).expect("a decimal"),
        )));
        let (answer, _) = decode_outcome(&encode_outcome(&held, &unnamed()), 0).expect("an answer");
        match answer {
            Answer::Value {
                value: Value::Number(Number::Decimal(read)),
                ..
            } => {
                assert_eq!(read.to_string(), "12.34");
            }
            other => panic!("a decimal came back as {other:?}"),
        }
    }

    #[test]
    fn a_reference_arrives_with_the_name_a_client_cannot_look_up() {
        // The catalog is on the server. Without this the client renders
        // `<record 3:7>`, which is not something anybody can paste back.
        let table = TableId::new(3);
        let mut names = Names::new();
        names.insert(table, "orders".to_owned());
        let held = Outcome::Records {
            records: vec![(
                RecordId::Int(1),
                Value::Record(RecordRef::new(table, RecordId::Int(7))),
            )],
            path: AccessPath::Record,
        };
        let (answer, used) = decode_outcome(&encode_outcome(&held, &names), 0).expect("an answer");
        assert_eq!(used, encode_outcome(&held, &names).len());
        match answer {
            Answer::Records { names: read, .. } => {
                assert_eq!(read.get(&table).map(String::as_str), Some("orders"));
            }
            other => panic!("records came back as {other:?}"),
        }
    }

    #[test]
    fn several_outcomes_read_back_in_order_from_one_body() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_outcome(&Outcome::Done, &unnamed()));
        body.extend_from_slice(&encode_outcome(&Outcome::Removed { count: 3 }, &unnamed()));
        body.extend_from_slice(&encode_outcome(
            &Outcome::Value(Value::from("last")),
            &unnamed(),
        ));

        let (first, at) = decode_outcome(&body, 0).expect("first");
        let (second, at) = decode_outcome(&body, at).expect("second");
        let (third, at) = decode_outcome(&body, at).expect("third");
        assert_eq!(first, Answer::Done);
        assert_eq!(second, Answer::Removed(3));
        assert_eq!(
            third,
            Answer::Value {
                value: Value::from("last"),
                names: unnamed(),
            }
        );
        assert_eq!(at, body.len());
    }
}
