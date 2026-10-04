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

mod outcome;
use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_ql::Parameters;
// `AccessPath` and `Outcome` are what the node encodes *from*; a client only
// ever decodes into `Answer`, so neither name reaches the client half.
#[cfg(feature = "server")]
use tessari_session::Outcome;
use tessari_types::{RecordId, TableId, Value};

use crate::error::{Error, Result};
use crate::frame::{put_bytes, put_text, put_u32, take_bytes, take_text, take_u32};
pub use outcome::spell;
pub(crate) use outcome::{decode_outcome_body, encode_outcome_body};

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

/// The names, as a count and that many pairs.
#[cfg(feature = "server")]
fn put_names(body: &mut Vec<u8>, names: &Names) {
    put_u32(body, u32::try_from(names.len()).unwrap_or(u32::MAX));
    for (table, name) in names {
        put_u32(body, table.get());
        put_text(body, name);
    }
}

// The node's encoding half is what these exercise — `encode_outcome`, the
// access-path tag, `named` — so they belong to the same feature it does.
#[cfg(all(test, feature = "server"))]
mod tests;
