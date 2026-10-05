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

use tessari_constants::SESSION_TOKEN_SECONDS;
use tessaridb::{AccessPath, Db, Error, Outcome};

use crate::basic::Presented;
use crate::json;
use crate::tokens::{Refused, Tokens};
pub use failure::refusal_kind;
pub(crate) use failure::{coded, failure, script_failure};
pub(crate) use metrics::metrics;
pub(crate) use scripts::{listing, script};

/// One answer: a status and a JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
    /// What the body is, as a `Content-Type`.
    ///
    /// JSON for everything the language answers, and octets for a file. A file
    /// is bytes the store has no opinion about — it did not ask what they were
    /// when they went in, so it does not claim to know coming out.
    pub kind: &'static str,
    /// Where this answer sends the caller instead, as a `Location`.
    ///
    /// `None` for every answer that is about this request, which is almost all
    /// of them. It is a field rather than a decision at the writer because the
    /// address is per-answer data — unlike the `WWW-Authenticate` challenge
    /// beside a `401`, which is a constant and therefore follows from the status
    /// alone.
    ///
    /// RFC 9110 is why it exists at all: a `307` without a `Location` is not a
    /// redirect a client can act on, exactly as a `401` without a challenge is
    /// not a `401` a client can act on.
    pub location: Option<String>,
    /// Whether the redirect in [`Self::location`] is settled — a leadership
    /// the caller may remember — when there is one, for the surface's count
    /// (G053 C6).
    pub settled: Option<bool>,
    /// The strong tag of an answer that may be kept but must be asked about
    /// again before it is reused — the console's assets, whose names carry no
    /// hash of their bytes. Present means `ETag` plus `Cache-Control: no-cache`.
    pub tag: Option<&'static str>,
}

/// What an answer says it is.
pub(crate) const JSON: &str = "application/json";
pub(crate) const OCTETS: &str = "application/octet-stream";
/// The exposition format's own content type, version and all — a scraper reads
/// the version to know how to parse, so naming it `text/plain` alone would be a
/// smaller true statement that costs the reader the useful half.
pub(crate) const EXPOSITION: &str = "text/plain; version=0.0.4; charset=utf-8";

impl Answer {
    /// An answer with this status and JSON body.
    #[must_use]
    pub fn new(status: u16, body: String) -> Self {
        Self {
            status,
            body: body.into_bytes(),
            kind: JSON,
            location: None,
            settled: None,
            tag: None,
        }
    }

    /// An answer that is text of some other kind than JSON.
    #[must_use]
    pub fn text(status: u16, body: String, kind: &'static str) -> Self {
        Self {
            status,
            body: body.into_bytes(),
            kind,
            location: None,
            settled: None,
            tag: None,
        }
    }

    /// An answer carrying a file's bytes.
    #[must_use]
    pub const fn octets(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            kind: OCTETS,
            location: None,
            settled: None,
            tag: None,
        }
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
        Err(error) => return failure(&tessaridb::Error::from(error)),
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

/// `GET /ready` — whether this node will take new work **now**.
///
/// A different question from `/health`, and the difference is what a supervisor
/// acts on: a probe that fails here means *stop sending traffic*, and a probe
/// that fails on liveness means *restart it*. Those are opposite instructions,
/// so a node that answers only one of them gets one of them wrong.
///
/// Two states make this false. The node is **leaving** — stage 0 of a staged
/// shutdown, where it is still serving and no longer wants new work. Or the
/// store is **unwell**, which is [`health`]'s own answer, delegated rather than
/// re-derived: one store, one opinion about it, and a node that is ready and
/// unhealthy at the same time would be a bug in the reporting rather than a
/// state a caller has to understand.
///
/// So when nothing is leaving, this answers exactly what `/health` answers. The
/// routes differ only where they are meant to.
pub(crate) fn ready(db: &Db, willing: bool) -> Answer {
    if willing {
        health(db)
    } else {
        Answer::new(503, r#"{"status":"leaving"}"#.to_owned())
    }
}

/// `POST /script` — run it, and answer with one object per statement.
///
/// The credential, when there is one, is presented **before** the script runs.
/// A request against an open store may carry none, which is what keeps an empty
/// store usable; a request against a closed one that carries none is answered
/// `401` by the session's own refusal, not by a second rule here.
/// A session, as whoever the request says it is.
///
/// Shared by the script route and the object routes rather than written twice,
/// because "who is asking" must be one answer: two sign-in paths is two places a
/// refusal can be forgotten. The token path is a third *claim* and not a third
/// path — it lands in the same session, established the same way, so a rule
/// added below binds all three.
///
/// A request against an open store may present nothing, which is what keeps an
/// empty store usable; one against a closed store that presents nothing is
/// answered `401` by the session's own refusal, not by a second rule here.
pub(crate) fn session_for<'a>(
    db: &'a Db,
    tokens: &Tokens,
    presented: &Presented,
) -> Result<tessaridb::Session<'a>, Answer> {
    let mut session = db.session();
    match presented {
        Presented::Nobody => {}
        Presented::Password(credentials) => {
            if let Err(error) = session.sign_in(&credentials.name, &credentials.password) {
                return Err(failure(&error));
            }
        }
        Presented::Token(bearer) => {
            // A token this node never issued and one whose account has moved are
            // the same answer, for the same reason a wrong name and a wrong
            // password are: the difference is only ever useful to somebody
            // holding a token they should not have.
            let Some(ticket) = tokens.holder(bearer) else {
                return Err(failure(&Error::TicketStale));
            };
            if let Err(error) = session.resume(&ticket) {
                // Dropped rather than left to expire. It can never work again —
                // the record it stands for has moved — so keeping it is holding
                // a row that exists only to be refused.
                tokens.forget(bearer);
                return Err(failure(&error));
            }
        }
    }
    Ok(session)
}

/// `POST /session` — check a password once and hand back a token.
///
/// The whole point of the route: a password is verified here and then not
/// again, so a client's second request costs a hash-map lookup instead of
/// nineteen mebibytes of Argon2.
///
/// Only a password may be exchanged for a token. Presenting a token to get
/// another one would make the first one's expiry meaningless — a holder could
/// roll it forward for as long as they kept asking, and the account would never
/// come back under the control of whoever owns the password.
pub(crate) fn open_session(db: &Db, presented: &Presented, tokens: &Tokens) -> Answer {
    let Presented::Password(credentials) = presented else {
        return Answer::new(
            401,
            r#"{"error":"present a name and password to open a session"}"#.to_owned(),
        );
    };
    let mut session = db.session();
    if let Err(error) = session.sign_in(&credentials.name, &credentials.password) {
        return failure(&error);
    }
    // An open store signs anybody in as nobody, and a token for nobody would
    // outlive the store's openness: declaring the first user closes the store,
    // and a token minted before that must not still work after it.
    let Some(ticket) = session.ticket() else {
        return Answer::new(
            401,
            r#"{"error":"this store has no users, so there is no session to open"}"#.to_owned(),
        );
    };
    match tokens.issue(ticket) {
        Ok(bearer) => {
            tracing::info!(user = %credentials.name, "a session was opened");
            Answer::new(
                200,
                format!(r#"{{"token":"{bearer}","expires_in":{SESSION_TOKEN_SECONDS}}}"#),
            )
        }
        Err(Refused::Full) => Answer::new(
            503,
            r#"{"error":"this node is holding as many sessions as it will"}"#.to_owned(),
        ),
    }
}

/// `POST /password` — change your own password, proving the current one.
///
/// **Basic only, never a token.** The whole point of the route is the second
/// proof: a token can be copied off a plaintext connection or read out of a
/// log, and a route that let one set a new password would make a stolen token a
/// permanent takeover rather than a temporary one.
///
/// The body is the new password and nothing else. It is not a script, so it
/// carries no grammar a value could be read as, and it is the same shape as
/// every other route here that takes one thing.
pub(crate) fn change_password(db: &Db, presented: &Presented, body: &str) -> Answer {
    let Presented::Password(credentials) = presented else {
        return Answer::new(
            401,
            r#"{"error":"present your name and current password to change it"}"#.to_owned(),
        );
    };
    let mut session = db.session();
    if let Err(error) = session.sign_in(&credentials.name, &credentials.password) {
        return failure(&error);
    }
    match session.change_password(&credentials.password, body) {
        Ok(()) => {
            // Every token this user held stopped working the moment the record
            // changed, so a client holding one has to sign in again — and is
            // told so here rather than discovering it on its next request.
            tracing::info!(user = %credentials.name, "a user changed their own password");
            Answer::new(200, r#"{"changed":true,"tokens_ended":true}"#.to_owned())
        }
        Err(error) => failure(&error),
    }
}

/// `DELETE /session` — forget the token this request carries.
///
/// Answers the same whether the token was here or not. Whether a token this
/// node never issued *existed* is not something the caller presenting it should
/// be able to learn, and there is nothing useful a client does differently.
pub(crate) fn close_session(presented: &Presented, tokens: &Tokens) -> Answer {
    if let Presented::Token(bearer) = presented {
        tokens.forget(bearer);
    }
    Answer::new(200, r#"{"closed":true}"#.to_owned())
}

/// `GET /backup` — a snapshot of the store's current state (ADR-0094 D1),
/// `?as=log` for the store's log, `?from=<n>` for the log's records since a
/// sequence, or `?as=script` for the state as TessariQL (ADR-0091).
///
/// A surface over the `BACKUP` statement rather than a second implementation of
/// it, so who may take one is decided in one place: the statement needs an
/// owner, and a grant-governed user is refused by name. This route adds no
/// permission of its own and must not — a backup is every table at once, and an
/// endpoint that decided that for itself would be a second answer to a question
/// the language already answers.
pub(crate) fn backup(
    db: &Db,
    query: Option<&str>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    // A query string that is not one this route takes is a mistake worth
    // naming: silently backing the whole store up when the caller asked for an
    // increment, or for a snapshot, is a very expensive typo.
    let script = match query {
        None => "BACKUP;".to_owned(),
        Some("as=state") => "BACKUP STATE;".to_owned(),
        Some("as=log") => "BACKUP LOG;".to_owned(),
        Some("as=script") => "BACKUP SCRIPT;".to_owned(),
        Some(written) => match written.strip_prefix("from=").map(str::parse::<u64>) {
            Some(Ok(held)) => format!("BACKUP FROM {held};"),
            _ => {
                return Answer::bad_request(
                    "this route answers a snapshot of the current state, or takes \
                     `as=log` for the log, `from=<sequence>` for the log since a \
                     position, or `as=script` for the state as TessariQL",
                );
            }
        },
    };
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    match session.run(&script) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Value(tessaridb::Value::Bytes(bytes))) => {
                Answer::octets(200, bytes.clone())
            }
            Some(Outcome::Value(tessaridb::Value::String(script))) => {
                Answer::octets(200, script.clone().into_bytes())
            }
            _ => Answer::new(
                500,
                r#"{"error":"the backup answered with no bytes"}"#.to_owned(),
            ),
        },
        Err(error) => failure(&error),
    }
}

/// The access path, reported because a scan should be visible rather than
/// folklore — the same reason the embedded surface carries it.
const fn name_of(path: AccessPath) -> &'static str {
    path.name()
}

#[cfg(test)]
mod corpus;
mod failure;
mod metrics;
pub(crate) mod scripts;
pub(crate) mod series;
mod topics;
pub(crate) mod vault;

#[cfg(test)]
mod tests;
