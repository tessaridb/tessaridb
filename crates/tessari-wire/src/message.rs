//! What a request and an answer hold.
//!
//! # Values travel in the store's own encoding
//!
//! Not JSON. The HTTP endpoint speaks JSON because a browser is owed JSON, and
//! it pays for that: seventeen value types projected onto six, with a decimal
//! quoted so it is not silently a double and a datetime, a duration and a record
//! reference all arriving as text. A client reading that back has to *guess* —
//! deciding `"12.34"` is a decimal and `"2s"` a duration by looking at them,
//! which is a parser inventing what a lossless encoder already knew.
//!
//! So a value here goes through `tessari_encoding::payload`, the same codec the
//! store writes records with. Seventeen types out, seventeen types back, nothing
//! to get wrong at either end.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_ql::Parameters;
// `AccessPath` and `Outcome` are what the node encodes *from*; a client only
// ever decodes into `Answer`, so neither name reaches the client half.
#[cfg(feature = "server")]
use tessari_session::{AccessPath, Outcome, Suggestion};
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
    let inner = encode_outcome_body(outcome, names);
    // The length in front is what makes an unknown outcome survivable: a client
    // that does not recognise the tag reads the length, yields `Unknown`, steps
    // over the rest and carries on with the next outcome. Without it a newer
    // node introducing an outcome kind anywhere in an answer breaks every older
    // client, because the reader has no way to find where the next one starts.
    let mut body = Vec::with_capacity(inner.len().saturating_add(4));
    put_u32(&mut body, u32::try_from(inner.len()).unwrap_or(u32::MAX));
    body.extend_from_slice(&inner);
    body
}

/// The outcome itself, from its tag onward — everything the length counts.
#[cfg(feature = "server")]
fn encode_outcome_body(outcome: &Outcome, names: &Names) -> Vec<u8> {
    let mut body = Vec::new();
    match outcome {
        Outcome::Done => body.push(tag::DONE),
        // The notes go **last**, after the records, and that placement is the
        // whole of their compatibility story. An outcome is length-prefixed and
        // the reader advances by the declared length rather than by what it
        // consumed, so bytes appended at the end are bytes an older client steps
        // over — the same mechanism that lets it survive an outcome tag it has
        // never heard of. Put anywhere else they would shift the offsets of
        // fields an older client does know how to read.
        Outcome::Records {
            records,
            plan,
            notes,
            suggestion,
            only,
        } => {
            body.push(tag::RECORDS);
            body.push(path_tag(plan.access));
            put_names(&mut body, names);
            put_u32(&mut body, u32::try_from(records.len()).unwrap_or(u32::MAX));
            for (id, value) in records {
                put_text(&mut body, &spell(id));
                put_bytes(&mut body, encode_payload(value).as_slice());
            }
            // A kind and a rendered message rather than the typed note. `Answer`
            // is deliberately not `Outcome` — a record id on this wire is text
            // too — and a client's two uses are to group by the kind and to show
            // the message, both of which the store already writes.
            put_u32(&mut body, u32::try_from(notes.len()).unwrap_or(u32::MAX));
            for note in notes {
                put_text(&mut body, note.kind());
                put_text(&mut body, &note.message());
            }
            // After the notes, for the reason the notes are after the records:
            // a client that stops before it reads `false`, which is the truth
            // about every read written by somebody who has never heard of
            // `ONLY`.
            body.push(u8::from(*only));
            // And exactness last, where the newest field goes — but read the
            // note on `Exact` before assuming the usual absent-means-default
            // rule applies to it. It does not, and this is the one field on this
            // wire for which it must not.
            body.push(u8::from(!plan.exact.is_exact()));
            put_text(&mut body, plan.exact.reason().unwrap_or_default());
            // The suggestion last, as the newest field. Its three states get
            // three distinct byte values rather than a flag plus an empty list,
            // because the difference this field exists to carry is exactly the
            // one a flag would lose: `0` is *no dictionary was asked*, `1` is *a
            // dictionary was asked and holds every term*, and `2` is *these
            // terms it does not*. A client that reads `0` where it meant `1`
            // reports a confident negative nobody checked.
            //
            // Which also decides what an older client's silence means. It stops
            // before this byte and so reads no suggestion at all — the honest
            // outcome, and the reason absent is `0` rather than any of the three
            // being the implicit default.
            match suggestion {
                None => body.push(0),
                Some(Suggestion::NothingNearer) => body.push(1),
                Some(Suggestion::DidYouMean(nearest)) => {
                    body.push(2);
                    put_u32(&mut body, u32::try_from(nearest.len()).unwrap_or(u32::MAX));
                    for correction in nearest {
                        put_text(&mut body, &correction.typed);
                        put_text(&mut body, &correction.instead);
                    }
                }
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
                put_text(&mut body, &spell(key));
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
        /// What the store did that the records alone do not show.
        ///
        /// Empty for almost every read, and empty too when the node answering
        /// is older than this client — a body that ends before the notes is a
        /// node that had none to send, not a malformed one.
        notes: Vec<Remark>,
        /// Whether the read said `ONLY`, so `records` holds the one record the
        /// caller asked about rather than a list to unwrap.
        ///
        /// `false` from a node older than this client, which is what every read
        /// such a node serves actually is.
        only: bool,
        /// Whether the node called the answer provably the one the question
        /// names — and `None` when it did not say.
        ///
        /// The one field on this wire that does not follow the rule above.
        /// Every other absent field reads as its default because the default is
        /// what an older node's read actually was; here the default would be a
        /// *claim*, and a node that predates the field made no claim at all.
        exact: Option<Exact>,
        /// What the node suggested the query might have meant — and `None` when
        /// it did not say, which is every node older than the field.
        ///
        /// The same shape as `exact` and for the same reason. A node that never
        /// asked a dictionary and a node too old to have the question are not
        /// the same fact, and collapsing them would let a client report "nothing
        /// is near" on behalf of a node that never looked.
        suggestion: Option<Suggested>,
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

/// What a node said about whether its answer is exact.
///
/// Two states rather than a `bool` because there is a third, and it lives one
/// level up as the `Option` around this: a node that never sent the field. That
/// separation is the whole reason the type exists — a client holding
/// `Option<bool>` writes `unwrap_or(true)` sooner or later, and the value it
/// invents there is a promise nobody on the other end made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exact {
    /// The node called the answer provably the one the question names.
    Yes,
    /// It did not, and said why.
    No {
        /// The reason, in the node's own words.
        reason: String,
    },
}

/// One term a query asked for that nothing holds, and the nearest that is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The term as the query asked for it, analyzed.
    pub typed: String,
    /// The nearest term the collection holds.
    pub instead: String,
}

/// What a node said the query might have meant.
///
/// Three states, and the `Option` around this carries a fourth. They are kept
/// apart for the reason [`Exact`] gives: the difference between *asked and found
/// nothing* and *never asked* is the whole value of the field, and a client
/// holding one flag invents the distinction back with a default nobody promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suggested {
    /// No term dictionary was consulted — the read had no search index to ask.
    ///
    /// Not a statement that nothing is near. Nothing was looked for.
    NotSought,
    /// A dictionary was consulted and holds every term the query named.
    NothingNearer,
    /// Terms it does not hold, each with the nearest one it does.
    ///
    /// Never empty from a node that follows the protocol; a node sending an
    /// empty list here has said [`Self::NothingNearer`] the long way, and a
    /// client is entitled to read it as that rather than as a correction.
    DidYouMean(Vec<Correction>),
}

/// One thing the store said about how it answered.
///
/// A kind and a message rather than the store's typed note, because a client's
/// two uses are to group by the first and show the second, and a typed note
/// would put the store's whole note vocabulary in every client's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remark {
    /// A short stable name, for grouping and filtering.
    pub kind: String,
    /// The note in the words a reader would want it in.
    pub message: String,
}

/// Read one outcome back, and how much of the buffer it used.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the body does not hold what it claims.
pub fn decode_outcome(body: &[u8], at: usize) -> Result<(Answer, usize)> {
    let (length, start) = take_u32(body, at)?;
    let length = usize::try_from(length).map_err(|_| Error::Malformed)?;
    let end = start.checked_add(length).ok_or(Error::Malformed)?;
    let inner = body.get(start..end).ok_or(Error::Malformed)?;
    // The next outcome begins where the length said it would, whatever the
    // decode below consumed. That is the whole point of the length: an outcome
    // this build has no name for costs it the bytes and not the connection.
    Ok((decode_outcome_body(inner)?, end))
}

/// One outcome, from its tag onward, in exactly the bytes its length claimed.
fn decode_outcome_body(body: &[u8]) -> Result<Answer> {
    let tag = body.first().copied().ok_or(Error::Malformed)?;
    let mut at = 1_usize;
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
            // Only if there are bytes left. A node older than this build sends
            // a body that ends here, and that is a node with nothing to say
            // rather than a short read — the one direction the length prefix
            // does not cover on its own.
            let notes = if at < body.len() {
                let (count, next) = take_u32(body, at)?;
                at = next;
                let mut notes = Vec::new();
                for _ in 0..count {
                    let (kind, next) = take_text(body, at)?;
                    let (message, next) = take_text(body, next)?;
                    at = next;
                    notes.push(Remark { kind, message });
                }
                notes
            } else {
                Vec::new()
            };
            // Same rule one field further along: absent means `false`, which is
            // what an older node's every read was.
            let only = body.get(at).copied().unwrap_or(0) != 0;
            at = at.saturating_add(1);
            // And here the rule stops. An absent exactness byte does **not**
            // mean the answer was exact: it means the node never said, and
            // reading it as `true` would put a claim in an older node's mouth on
            // the one property that exists precisely so it is never inferred.
            // `None` is a third answer and a caller has to handle it.
            let exact = if at < body.len() {
                let approximate = body.get(at).copied().ok_or(Error::Malformed)? != 0;
                let (reason, next) = take_text(body, at.saturating_add(1))?;
                at = next;
                Some(if approximate {
                    Exact::No { reason }
                } else {
                    Exact::Yes
                })
            } else {
                None
            };
            // And the same rule again, one field further along, because the
            // reason for it is the same: silence here is a node that never had
            // the question, not a node reporting that nothing was near.
            let suggestion = if at < body.len() {
                let state = body.get(at).copied().ok_or(Error::Malformed)?;
                at = at.saturating_add(1);
                match state {
                    0 => Some(Suggested::NotSought),
                    1 => Some(Suggested::NothingNearer),
                    2 => {
                        let (count, next) = take_u32(body, at)?;
                        at = next;
                        let mut corrections = Vec::new();
                        for _ in 0..count {
                            let (typed, next) = take_text(body, at)?;
                            let (instead, next) = take_text(body, next)?;
                            at = next;
                            corrections.push(Correction { typed, instead });
                        }
                        Some(Suggested::DidYouMean(corrections))
                    }
                    // A state this build does not know is not a malformed
                    // answer — it is a newer node saying something in a
                    // vocabulary this client lacks, and the honest reading of
                    // that is the same as silence. The length prefix already
                    // carries the reader past whatever followed.
                    _ => None,
                }
            } else {
                None
            };
            Answer::Records {
                records,
                path,
                names,
                notes,
                only,
                exact,
                suggestion,
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
    // `at` has done its work inside this outcome; the caller advances by the
    // declared length instead, so a short read here cannot desynchronise the
    // stream.
    let _ = at;
    Ok(answer)
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
        AccessPath::Approximate => 4,
        AccessPath::Graph => 5,
        AccessPath::Join => 6,
        AccessPath::Materialised => 7,
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
        4 => "approximate",
        5 => "graph",
        6 => "join",
        7 => "materialised",
        _ => "scan",
    }
}

/// The identity of a record, as text.
///
/// Kept as the store spells it rather than re-parsed, because a client that
/// wants to name a record writes that text into its next script — and a second
/// reading of a record id is a second place for the two to disagree.
///
/// The spelling is the language's, which is what makes the sentence above true:
/// this returned `Display` until the wave that measured it, and `Display` writes
/// a UUID as thirty-two undivided digits and a text identity unquoted, neither
/// of which stands where the grammar puts an identity. A client following the
/// protocol to the letter got back text that its next script could not read.
///
/// Every place that puts an identity on this wire calls this. It returned the
/// wrong form partly because nothing called it at all — the encoder stringified
/// the id itself, so the one function documenting the promise was not the one
/// keeping it.
#[must_use]
pub fn spell(id: &RecordId) -> String {
    id.to_literal()
}

// The node's encoding half is what these exercise — `encode_outcome`, the
// access-path tag, `named` — so they belong to the same feature it does.
#[cfg(all(test, feature = "server"))]
mod tests {
    #![allow(clippy::panic)]

    use tessari_session::{AccessPath, Outcome, Parameters, Plan};
    use tessari_types::{Number, RecordId, RecordRef, TableId, Value};

    use super::{
        Answer, Correction, Exact, Names, Remark, Request, Suggested, decode_outcome,
        encode_outcome,
    };

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
                plan: Plan::new(AccessPath::Index),
                notes: Vec::new(),
                suggestion: None,
                only: false,
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
            plan: Plan::new(AccessPath::Record),
            notes: Vec::new(),
            suggestion: None,
            only: false,
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

    /// One read that has something to say, for the compatibility tests below.
    fn noted() -> Outcome {
        Outcome::Records {
            records: vec![(RecordId::Int(7), Value::from("ada"))],
            plan: Plan::new(AccessPath::Scan),
            notes: vec![tessari_session::Note::Approximate],
            suggestion: None,
            only: false,
        }
    }

    #[test]
    fn a_note_survives_the_wire() {
        let body = encode_outcome(&noted(), &unnamed());
        let (answer, used) = decode_outcome(&body, 0).expect("an answer");
        assert_eq!(used, body.len());
        let Answer::Records { notes, records, .. } = answer else {
            panic!("not records")
        };
        // The records are still there, which is the half a note must never cost.
        assert_eq!(records.len(), 1);
        assert_eq!(
            notes,
            vec![Remark {
                kind: "approximate".to_owned(),
                message: "an approximate index answered this, so a nearer record may exist"
                    .to_owned(),
            }]
        );
    }

    #[test]
    fn a_newer_node_does_not_break_an_older_client() {
        // The older client is modelled by its behaviour rather than by an old
        // build: it reads the fields it knows and then advances by the *declared
        // length*, which is what `decode_outcome` returns. A body carrying notes
        // it never heard of costs it those bytes and not the connection.
        let plain = encode_outcome(
            &Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                plan: Plan::new(AccessPath::Scan),
                notes: Vec::new(),
                suggestion: None,
                only: false,
            },
            &unnamed(),
        );
        let noted = encode_outcome(&noted(), &unnamed());
        assert!(noted.len() > plain.len(), "the notes were not encoded");
        // Two outcomes back to back: the second is found only if the first's
        // extra bytes were stepped over correctly, which is the property an
        // appended field actually needs.
        let mut stream = noted.clone();
        stream.extend_from_slice(&plain);
        let (_, after) = decode_outcome(&stream, 0).expect("the first");
        assert_eq!(after, noted.len(), "the notes desynchronised the stream");
        let (second, end) = decode_outcome(&stream, after).expect("the second");
        assert_eq!(end, stream.len());
        assert!(matches!(second, Answer::Records { .. }));
    }

    #[test]
    fn an_older_node_does_not_break_a_newer_client() {
        // The direction the length prefix does *not* cover on its own. An older
        // node sends a body that simply ends after the records, and this build
        // must read that as a node with nothing to say rather than as a short
        // read.
        let full = encode_outcome(
            &Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                plan: Plan::new(AccessPath::Scan),
                notes: Vec::new(),
                suggestion: None,
                only: false,
            },
            &unnamed(),
        );
        // Drop the tail the current encoder writes and shrink the declared
        // length to match, which is exactly the body an older node would have
        // produced.
        //
        // The constants are named rather than summed into a literal so that the
        // *reason* for the number survives — but naming them does not make this
        // safe on its own, and the exactness field proved it: appending a field
        // and leaving this alone compiles perfectly and silently retargets the
        // slice at the middle of the tail rather than its start. What actually
        // catches that is the assertion below that the decoded answer has
        // **none** of the appended fields, which fails the moment one of them
        // survives the truncation.
        const NOTE_COUNT: usize = 4;
        const ONLY_FLAG: usize = 1;
        // A tag byte and the four-byte length of an empty reason.
        const EXACTNESS: usize = 1 + 4;
        // One state byte. `None` writes nothing after it, so this is the whole
        // of the field for a read that consulted no dictionary.
        const SUGGESTION: usize = 1;
        let inner = &full[4..full.len() - NOTE_COUNT - ONLY_FLAG - EXACTNESS - SUGGESTION];
        let mut older = Vec::new();
        older.extend_from_slice(&u32::try_from(inner.len()).expect("small").to_be_bytes());
        older.extend_from_slice(inner);
        let (answer, used) = decode_outcome(&older, 0).expect("an older answer");
        assert_eq!(used, older.len());
        let Answer::Records {
            notes,
            records,
            only,
            exact,
            suggestion,
            ..
        } = answer
        else {
            panic!("not records")
        };
        assert_eq!(records.len(), 1, "an older body lost its records");
        assert!(notes.is_empty(), "notes appeared from nowhere");
        assert!(!only, "a body that ends early claimed to be an `ONLY` read");
        // And the one field where absence is **not** its default. A node that
        // predates exactness made no claim about it, and reading the silence as
        // `true` would put a promise in its mouth on the one property whose
        // whole purpose is that it is never inferred.
        assert!(
            exact.is_none(),
            "a body that ends early claimed its answer was exact",
        );
        // And the same again for the newest field, where the mistake would be
        // worse: reading silence as `NotSought` is nearly right and completely
        // unfounded, and reading it as `NothingNearer` would have this client
        // report on a dictionary the older node never had.
        assert!(
            suggestion.is_none(),
            "a body that ends early said something about a suggestion",
        );
    }

    /// The suggestion's three states survive the wire as three states.
    ///
    /// The encoding gives each its own byte rather than a flag plus a possibly
    /// empty list, and this is what holds it to that: an implementation that
    /// wrote `NothingNearer` as an empty `DidYouMean` would pass every test
    /// about corrections and fail here, which is the right place for it to fail
    /// because the difference is the whole point of the field.
    #[test]
    fn a_suggestion_crosses_the_wire_as_three_states_and_not_two() {
        let crossed = |suggestion| {
            let encoded = encode_outcome(
                &Outcome::Records {
                    records: vec![(RecordId::Int(7), Value::from("ada"))],
                    plan: Plan::new(AccessPath::Scan),
                    notes: Vec::new(),
                    suggestion,
                    only: false,
                },
                &unnamed(),
            );
            let (answer, used) = decode_outcome(&encoded, 0).expect("a suggestion");
            assert_eq!(
                used,
                encoded.len(),
                "the suggestion desynchronised the stream"
            );
            let Answer::Records { suggestion, .. } = answer else {
                panic!("not records")
            };
            suggestion
        };

        assert_eq!(crossed(None), Some(Suggested::NotSought));
        assert_eq!(
            crossed(Some(tessari_session::Suggestion::NothingNearer)),
            Some(Suggested::NothingNearer),
            "a dictionary that was asked crossed as one that was not"
        );
        assert_eq!(
            crossed(Some(tessari_session::Suggestion::DidYouMean(vec![
                tessari_session::Nearest {
                    typed: "vecter".to_owned(),
                    instead: "vector".to_owned(),
                }
            ]))),
            Some(Suggested::DidYouMean(vec![Correction {
                typed: "vecter".to_owned(),
                instead: "vector".to_owned(),
            }])),
        );
    }

    /// The three states, told apart.
    ///
    /// A `bool` would collapse the first two of these into each other at the
    /// first `unwrap_or`, which is why the client's field is an `Option` around a
    /// two-state type rather than an `Option<bool>` — and why this test asserts
    /// all three rather than the interesting one.
    #[test]
    fn a_node_that_says_nothing_is_not_a_node_that_says_exact() {
        let said = |access| {
            let encoded = encode_outcome(
                &Outcome::Records {
                    records: vec![(RecordId::Int(7), Value::from("ada"))],
                    plan: Plan::new(access),
                    notes: Vec::new(),
                    suggestion: None,
                    only: false,
                },
                &unnamed(),
            );
            let (answer, used) = decode_outcome(&encoded, 0).expect("an answer");
            assert_eq!(used, encoded.len(), "exactness desynchronised the stream");
            let Answer::Records { exact, .. } = answer else {
                panic!("not records")
            };
            exact
        };

        assert_eq!(said(AccessPath::Scan), Some(Exact::Yes));
        let Some(Exact::No { reason }) = said(AccessPath::Approximate) else {
            panic!("the graph walk crossed the wire calling itself exact");
        };
        // The node's own words, carried rather than re-invented on this side: a
        // client that had to phrase the reason itself would be describing a read
        // it did not perform.
        assert_eq!(reason, tessari_session::Note::Approximate.message());
    }

    #[test]
    fn the_only_flag_survives_the_wire_and_does_not_desynchronise_a_stream() {
        let alone = encode_outcome(
            &Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                plan: Plan::new(AccessPath::Record),
                notes: Vec::new(),
                suggestion: None,
                only: true,
            },
            &unnamed(),
        );
        let (answer, used) = decode_outcome(&alone, 0).expect("an answer");
        assert_eq!(used, alone.len(), "the flag left bytes unread");
        assert!(
            matches!(answer, Answer::Records { only: true, .. }),
            "the flag did not survive"
        );
        // Back to back with a second outcome, which is the property an appended
        // field actually needs: the second is found only if the first's flag was
        // stepped over.
        let mut stream = alone.clone();
        stream.extend_from_slice(&alone);
        let (_, after) = decode_outcome(&stream, 0).expect("the first");
        assert_eq!(after, alone.len(), "the flag desynchronised the stream");
        let (second, end) = decode_outcome(&stream, after).expect("the second");
        assert_eq!(end, stream.len());
        assert!(matches!(second, Answer::Records { only: true, .. }));
    }
}
