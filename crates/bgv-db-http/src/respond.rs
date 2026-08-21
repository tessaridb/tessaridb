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
//! - **`500`** — a substrate or decoding failure. Nothing else reaches it, and
//!   anything that does is a bug rather than a user's mistake.
//!
//! That distinction is worth the mapping: a client can retry a `409` after
//! changing its data and can never fix a `400` that way.

use bgv_db::{AccessPath, Db, Error, Outcome};

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
pub(crate) fn health(db: &Db) -> Answer {
    match db.committed_tail() {
        Ok(tail) => Answer::new(
            200,
            format!(r#"{{"status":"ok","committed":{}}}"#, tail.get()),
        ),
        Err(error) => failure(&error),
    }
}

/// `POST /script` — run it, and answer with one object per statement.
pub(crate) fn script(db: &Db, source: &str) -> Answer {
    let mut session = db.session();
    match session.run(source) {
        Ok(outcomes) => {
            let mut body = String::from(r#"{"results":["#);
            for (position, outcome) in outcomes.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                encode(&mut body, outcome);
            }
            body.push_str("]}");
            Answer::new(200, body)
        }
        Err(error) => failure(&error),
    }
}

/// One outcome, as the object a caller parses.
fn encode(body: &mut String, outcome: &Outcome) {
    match outcome {
        Outcome::Done => body.push_str(r#"{"kind":"done"}"#),
        Outcome::Value(value) => {
            body.push_str(r#"{"kind":"value""#);
            // A `value` key that is absent means `none`, and one holding `null`
            // means `null`. JSON has one word for both, so the distinction is
            // carried by the key — see `json`.
            if value.is_present() {
                body.push_str(r#","value":"#);
                json::write(body, value);
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
                json::write(body, record);
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
