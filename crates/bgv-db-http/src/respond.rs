//! What each route answers, and with which status.
//!
//! # A status is not enough
//!
//! A script is many statements, so `400` alone makes its author read the whole
//! thing again. Every failure in this language already carries a span, because
//! that is what the diagnostics were built for — so the body names the failure
//! and where it is, and the status says what kind of failure it was.
//!
//! # The three kinds
//!
//! - **`400`** — the script could not be read, or asks for something the
//!   language does not have. The caller wrote it wrong.
//! - **`409`** — the store refused: a name taken, a unique value claimed twice,
//!   a schema violated, a required field left empty. The caller wrote it right
//!   and the data says no.
//! - **`401`** — this node does not know who is asking. Either no credential
//!   arrived against a closed store, or the one that did was refused.
//! - **`403`** — this node knows who is asking and the answer is still no: the
//!   role forbids the statement, or it reached outside its tenancy.
//! - **`500`** — a substrate or decoding failure. Nothing else reaches it, and
//!   anything that does is a bug rather than a user's mistake.
//!
//! That distinction is worth the mapping: a client can retry a `409` after
//! changing its data and can never fix a `400` that way.
//!
//! `401` and `403` are the same distinction one step earlier — "I do not know
//! you" against "I know you and no". A client that cannot tell them apart
//! retries a signin that will never help, or gives up on one that would.

use bgv_db::{AccessPath, Db, Error, Outcome};

use crate::basic::Credentials;
use crate::json;

/// One answer: a status and a JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// The body, always JSON.
    pub body: String,
}

impl Answer {
    /// An answer with this status and body.
    #[must_use]
    pub const fn new(status: u16, body: String) -> Self {
        Self { status, body }
    }

    /// A refusal naming what was wrong with the request.
    #[must_use]
    pub fn bad_request(reason: &str) -> Self {
        let mut body = String::from(r#"{"error":"#);
        json::string(&mut body, reason);
        body.push('}');
        Self::new(400, body)
    }
}

/// `GET /health` — whether this is a live node or a listening socket.
///
/// Carries the committed tail, because "the process is up" and "the store is
/// readable" are different claims and only the second one is useful.
///
/// # Why an unwell store fails this rather than raising an alert somewhere
///
/// An engine's compaction, flushing and write-ahead work happens on its own
/// threads, and a failure there surfaces at no call a caller makes: the store
/// keeps answering reads while the thing that keeps them has stopped. Something
/// has to ask, and something has to be told.
///
/// **This is the something.** A `503` here is taken out of rotation by every
/// load balancer and paged on by every monitor, so the alert is the one that
/// already exists rather than a second one written into this repository and
/// exercised never. The store reports what is true; this decides that an unwell
/// store should stop being sent traffic.
///
/// The body names the complaint, because a page that says only "unhealthy"
/// sends somebody to read code at three in the morning.
pub(crate) fn health(db: &Db) -> Answer {
    let held = match db.store().health() {
        Ok(held) => held,
        Err(error) => return failure(&bgv_db::Error::from(error)),
    };
    match held.complaint() {
        None => Answer::new(
            200,
            format!(r#"{{"status":"ok","committed":{}}}"#, held.committed.get()),
        ),
        Some(said) => Answer::new(
            503,
            format!(
                r#"{{"status":"unwell","committed":{},"background_errors":{},"complaint":{}}}"#,
                held.committed.get(),
                held.background_errors,
                crate::json::string_literal(&said),
            ),
        ),
    }
}

/// `POST /script` — run it, and answer with one object per statement.
///
/// The credential, when there is one, is presented **before** the script runs.
/// A request against an open store may carry none, which is what keeps an empty
/// store usable; a request against a closed one that carries none is answered
/// `401` by the session's own refusal, not by a second rule here.
pub(crate) fn script(db: &Db, source: &str, credentials: Option<&Credentials>) -> Answer {
    let mut session = db.session();
    if let Some(presented) = credentials
        && let Err(error) = session.sign_in(&presented.name, &presented.password)
    {
        return failure(&error);
    }
    match session.run(source) {
        Ok(outcomes) => {
            // Resolved once for the whole answer rather than per outcome, and
            // only when something in it holds a reference: a record reference
            // carries a table id, and a client receiving `"1:2"` cannot follow
            // it. See `Db::names_in`.
            let referenced: Vec<(bgv_db::RecordId, bgv_db::Value)> = outcomes
                .iter()
                .flat_map(|outcome| match outcome {
                    Outcome::Records { records, .. } => records.clone(),
                    Outcome::Value(held) => {
                        vec![(bgv_db::RecordId::Int(0), held.clone())]
                    }
                    _ => Vec::new(),
                })
                .collect();
            let names = db.names_in(&referenced).unwrap_or_default();

            let mut body = String::from(r#"{"results":["#);
            for (position, outcome) in outcomes.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                encode(&mut body, outcome, &names);
            }
            body.push_str("]}");
            Answer::new(200, body)
        }
        Err(error) => failure(&error),
    }
}

/// One outcome, as the object a caller parses.
fn encode(body: &mut String, outcome: &Outcome, names: &json::Names) {
    match outcome {
        Outcome::Done => body.push_str(r#"{"kind":"done"}"#),
        Outcome::Value(value) => {
            body.push_str(r#"{"kind":"value""#);
            // A `value` key that is absent means `none`, and one holding `null`
            // means `null`. JSON has one word for both, so the distinction is
            // carried by the key — see `json`.
            if value.is_present() {
                body.push_str(r#","value":"#);
                json::write(body, value, names);
            }
            body.push('}');
        }
        Outcome::Keys(keys) => {
            body.push_str(r#"{"kind":"keys","keys":["#);
            for (position, key) in keys.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                json::string(body, &key.to_string());
            }
            body.push_str("]}");
        }
        Outcome::Records { records, path } => {
            body.push_str(r#"{"kind":"records","path":"#);
            json::string(body, name_of(*path));
            body.push_str(r#","records":["#);
            for (position, (id, record)) in records.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                body.push_str(r#"{"id":"#);
                json::string(body, &id.to_string());
                body.push_str(r#","value":"#);
                json::write(body, record, names);
                body.push('}');
            }
            body.push_str("]}");
        }
        // `Outcome` is `#[non_exhaustive]`, so a shape this binary does not know
        // is possible in principle. Answering with its absence is honest;
        // guessing at its content would not be.
        _ => body.push_str(r#"{"kind":"unknown"}"#),
    }
}

/// The access path, reported because a scan should be visible rather than
/// folklore — the same reason the embedded surface carries it.
const fn name_of(path: AccessPath) -> &'static str {
    path.name()
}

/// A failure, as the status that says what kind it was.
fn failure(error: &Error) -> Answer {
    let status = match error {
        // This node does not know who is asking: no credential against a closed
        // store, or one it refused. Both are answered the same way, because
        // telling them apart tells an attacker which half to keep guessing at.
        Error::NotSignedIn { .. } | Error::SignInRefused => 401,
        // It knows, and the answer is still no. A different thing entirely, and
        // a client that cannot tell retries a signin that will never help.
        Error::RoleForbids { .. } | Error::OutsideTenancy { .. } => 403,
        // The caller wrote it wrong, and no amount of changing the data helps.
        Error::Script(_) => 400,
        // The caller wrote it right and the data says no. Retriable after a
        // change, which is the whole reason this is not a 400.
        Error::Store(_) => 409,
        // A substrate or decoding failure. Anything reaching here is a bug.
        Error::Encoding(_) => 500,
        // Everything else the session raises is about the script: an unselected
        // namespace, a wrong argument, a condition that is not a boolean.
        _ => 400,
    };
    let mut body = String::from(r#"{"error":"#);
    json::string(&mut body, &error.to_string());
    body.push('}');
    Answer::new(status, body)
}
