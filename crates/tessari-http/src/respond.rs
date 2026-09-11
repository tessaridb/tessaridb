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

use std::collections::BTreeMap;

use tessari_constants::SESSION_TOKEN_SECONDS;
use tessari_serve::{Census, Stopping};
use tessaridb::{AccessPath, Db, Error, Outcome};

use crate::basic::Presented;
use crate::json;
use crate::tokens::{Refused, Tokens};

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
        }
    }

    /// An answer that is text of some other kind than JSON.
    #[must_use]
    pub fn text(status: u16, body: String, kind: &'static str) -> Self {
        Self {
            status,
            body: body.into_bytes(),
            kind,
        }
    }

    /// An answer carrying a file's bytes.
    #[must_use]
    pub const fn octets(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            kind: OCTETS,
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

/// `GET /metrics` — the numbers, in the exposition format every scraper reads.
///
/// Plain text with a documented grammar, so it costs a function rather than a
/// dependency — which is the same trade the rest of this program makes and the
/// reason this format was chosen over any that needs a library to emit.
///
/// `# HELP` and `# TYPE` on every metric, because a scrape that describes itself
/// is the difference between a dashboard somebody can write and one that sends
/// them to read this file.
///
/// # What is absent, and why absent beats wrong
///
/// Without a [`Census`] — a node bound in-process, with no surrounding process
/// enumerating its surfaces — there is **no uptime line at all**, and the only
/// counters reported are this surface's own. The alternative would be to time
/// from this listener's own creation and call it uptime, which makes one metric
/// name mean two different things depending on how the node was started. A
/// scraper copes with a series that is missing; it cannot cope with one that
/// silently changes what it measures.
pub(crate) fn metrics(
    db: &Db,
    census: Option<&Census>,
    mine: &Stopping,
    tokens: &Tokens,
) -> Answer {
    let mut out = String::new();

    if let Some(census) = census {
        out.push_str("# HELP tessari_uptime_seconds How long this process has been running.\n");
        out.push_str("# TYPE tessari_uptime_seconds gauge\n");
        out.push_str(&format!(
            "tessari_uptime_seconds {:.3}\n",
            census.uptime().as_secs_f64()
        ));
    }

    // The store's own numbers, from the same call `/health` makes. A second way
    // to ask would be a second answer to drift from.
    if let Ok(held) = db.store().health() {
        out.push_str(
            "# HELP tessari_committed_sequence The last sequence the log has committed.\n",
        );
        out.push_str("# TYPE tessari_committed_sequence counter\n");
        out.push_str(&format!(
            "tessari_committed_sequence {}\n",
            held.committed.get()
        ));
        out.push_str("# HELP tessari_background_errors Failures in the engine's own threads.\n");
        out.push_str("# TYPE tessari_background_errors counter\n");
        out.push_str(&format!(
            "tessari_background_errors {}\n",
            held.background_errors
        ));
        // Any value above zero means this node was offered a record from a
        // leadership other than the one it applied at that position, and refused
        // it. It does not fall back to zero: the fork an operator most needs to
        // see is the one that stopped happening on its own.
        out.push_str(
            "# HELP tessari_log_forks Log positions another leadership tried to rewrite.\n",
        );
        out.push_str("# TYPE tessari_log_forks counter\n");
        out.push_str(&format!("tessari_log_forks {}\n", held.log_forks));
    }

    // Worth a line of its own because it is the one number that says whether
    // the token bound is close: a node at `MAX_SESSION_TOKENS` starts refusing
    // sign-ins while every other counter here still reads healthy.
    out.push_str("# HELP tessari_sessions Session tokens this node is holding.\n");
    out.push_str("# TYPE tessari_sessions gauge\n");
    out.push_str(&format!("tessari_sessions {}\n", tokens.held()));

    out.push_str("# HELP tessari_connections Requests in flight, by surface.\n");
    out.push_str("# TYPE tessari_connections gauge\n");
    out.push_str("# HELP tessari_subscriptions Feeds open, by surface.\n");
    out.push_str("# TYPE tessari_subscriptions gauge\n");
    out.push_str("# HELP tessari_answers_total Answers written, refusals included.\n");
    out.push_str("# TYPE tessari_answers_total counter\n");
    out.push_str(
        "# HELP tessari_refusals_total Answers that were a failure rather than a result.\n",
    );
    out.push_str("# TYPE tessari_refusals_total counter\n");
    out.push_str("# HELP tessari_ready Whether the surface will take new work.\n");
    out.push_str("# TYPE tessari_ready gauge\n");

    match census {
        Some(census) => {
            for (name, stopping) in census.surfaces() {
                surface(&mut out, name, stopping);
            }
        }
        None => surface(&mut out, "http", mine),
    }

    Answer::text(200, out, EXPOSITION)
}

/// One surface's five numbers, labelled by which surface it is.
fn surface(out: &mut String, name: &str, stopping: &Stopping) {
    // A label value is quoted and the names here are ours rather than a caller's,
    // so there is nothing to escape and no escaping written that would never run.
    out.push_str(&format!(
        "tessari_connections{{surface=\"{name}\"}} {}\n",
        stopping.requests()
    ));
    out.push_str(&format!(
        "tessari_subscriptions{{surface=\"{name}\"}} {}\n",
        stopping.feeds()
    ));
    out.push_str(&format!(
        "tessari_answers_total{{surface=\"{name}\"}} {}\n",
        stopping.answers()
    ));
    out.push_str(&format!(
        "tessari_refusals_total{{surface=\"{name}\"}} {}\n",
        stopping.refusals()
    ));
    out.push_str(&format!(
        "tessari_ready{{surface=\"{name}\"}} {}\n",
        u8::from(stopping.ready())
    ));
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
            log::info!("a session was opened for {}", credentials.name);
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
            log::info!("{} changed their own password", credentials.name);
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

/// `GET /backup` — the store's log as a backup file, or `?from=<n>` for the
/// records since a sequence.
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
    let from = match query {
        None => None,
        Some(written) => match written.strip_prefix("from=").map(str::parse::<u64>) {
            Some(Ok(held)) => Some(held),
            // A query string that is not the one parameter this route takes is a
            // mistake worth naming: silently backing the whole store up when the
            // caller asked for an increment is a very expensive typo.
            _ => {
                return Answer::bad_request("the only query this route takes is `from=<sequence>`");
            }
        },
    };
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let script = from.map_or_else(
        || "BACKUP;".to_owned(),
        |held| format!("BACKUP FROM {held};"),
    );
    match session.run(&script) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Value(tessaridb::Value::Bytes(bytes))) => {
                Answer::octets(200, bytes.clone())
            }
            _ => Answer::new(
                500,
                r#"{"error":"the backup answered with no bytes"}"#.to_owned(),
            ),
        },
        Err(error) => failure(&error),
    }
}

/// A bucket's listing: what the files are, and nothing about how they were
/// found.
///
/// This used to hand back the raw statement result wrapped in a key, which
/// published `plan.access`, `plan.table`, `kind` and `path` — planner internals
/// — plus `chunks`, a storage detail (Q-260). A public route's body is a
/// contract in every language a client is written in, so changing the planner
/// would then have broken clients that never asked about it.
///
/// A file listing wants a name, a size and a modification time, and the records
/// already carry exactly those three. A record missing one of them contributes
/// the keys it has rather than a null: the caller asked what is in the bucket,
/// and a key that is absent says the store never recorded it.
pub(crate) fn listing(outcomes: &[Outcome]) -> Answer {
    let records = outcomes
        .last()
        .and_then(Outcome::records)
        .unwrap_or_default();
    let names = json::Names::new();
    let mut body = String::from(r#"{"files":["#);
    for (position, (id, held)) in records.iter().enumerate() {
        if position > 0 {
            body.push(',');
        }
        body.push_str(r#"{"path":"#);
        json::string(&mut body, &id.to_string());
        if let tessaridb::Value::Object(fields) = held {
            for key in ["size", "updated"] {
                if let Some(value) = fields.get(key).filter(|value| value.is_present()) {
                    body.push(',');
                    json::string(&mut body, key);
                    body.push(':');
                    json::write(&mut body, value, &names);
                }
            }
        }
        body.push('}');
    }
    body.push_str("]}");
    Answer::new(200, body)
}

/// `POST /script` — run a script, with the values its parameters bind to.
///
/// Each value arrives **written in TessariQL** and is read by the language, which is
/// what keeps a supplied value from ever being read as grammar (SGA.T2): binding
/// happens after parsing and before the first statement, so `'; DROP TABLE
/// users; --` is a string that says something alarming rather than a statement.
/// A value that would not stand alone in a script is refused here, before
/// anything runs.
pub(crate) fn script(
    db: &Db,
    source: &str,
    written: &BTreeMap<String, String>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let mut given = tessaridb::Parameters::new();
    for (name, value) in written {
        match tessaridb::value_of(value) {
            Ok(held) => {
                given.insert(name.clone(), held);
            }
            Err(reason) => {
                return Answer::bad_request(&format!("parameter {name}: {reason}"));
            }
        }
    }
    match session.run_with(source, &given) {
        Ok(outcomes) => {
            // Resolved once for the whole answer rather than per outcome, and
            // only when something in it holds a reference: a record reference
            // carries a table id, and a client receiving `"1:2"` cannot follow
            // it. See `Db::names_in`.
            let referenced: Vec<(tessaridb::RecordId, tessaridb::Value)> = outcomes
                .iter()
                .flat_map(|outcome| match outcome {
                    Outcome::Records { records, .. } => records.clone(),
                    Outcome::Value(held) => {
                        vec![(tessaridb::RecordId::Int(0), held.clone())]
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
        Outcome::Records {
            records,
            plan,
            notes,
            suggestion,
            only,
        } => {
            body.push_str(r#"{"kind":"records","path":"#);
            json::string(body, name_of(plan.access));
            // The whole plan beside the one word, because the word alone cannot
            // say which index served the read. `path` stays: it is what every
            // client already reads, the two are rendered from the same field so
            // they cannot disagree, and removing it would break readers for
            // nothing.
            body.push_str(r#","plan":"#);
            json::write(body, &plan.to_value(), names);
            // Written only when there is something to say, so every response
            // that had nothing to report is byte-identical to what it was before
            // notes existed. A reader that wants them handles an absent key,
            // which every JSON reader already does.
            if !notes.is_empty() {
                body.push_str(r#","notes":["#);
                for (position, note) in notes.iter().enumerate() {
                    if position > 0 {
                        body.push(',');
                    }
                    body.push_str(r#"{"kind":"#);
                    json::string(body, note.kind());
                    body.push_str(r#","message":"#);
                    json::string(body, &note.message());
                    body.push('}');
                }
                body.push(']');
            }
            // Three states in two JSON facts, which is what lets this key stay
            // absent from the responses that never asked the question — every
            // read without a `MATCHES` over an indexed field, which is nearly
            // all of them.
            //
            // Absent means no term dictionary was consulted, and that is not a
            // claim about the collection: nothing was looked for. PRESENT AND
            // EMPTY is the claim — a dictionary was asked and holds every term
            // the query named. The two must not collapse, because a client that
            // reads an absent key as "nothing is near" is reporting a negative
            // the server never checked.
            if let Some(suggestion) = suggestion {
                body.push_str(r#","suggestion":{"corrections":["#);
                for (position, correction) in suggestion.corrections().iter().enumerate() {
                    if position > 0 {
                        body.push(',');
                    }
                    body.push_str(r#"{"typed":"#);
                    json::string(body, &correction.typed);
                    body.push_str(r#","instead":"#);
                    json::string(body, &correction.instead);
                    body.push('}');
                }
                body.push_str("]}");
            }
            // Written only when true, for the same reason the notes are written
            // only when there are some: every response from a read that did not
            // say `ONLY` stays byte-identical to what it was before the clause
            // existed. `records` stays an array holding at most one, because
            // changing a key's *type* would break every reader, and the flag is
            // what lets a reader that wants the record take it.
            if *only {
                body.push_str(r#","only":true"#);
            }
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
        // How many records a conditional delete removed, which is the whole
        // point of a retention statement — `done` would make the operator run a
        // count before and after to learn it.
        Outcome::Removed { count } => {
            body.push_str(r#"{"kind":"removed","count":"#);
            body.push_str(&count.to_string());
            body.push('}');
        }
        // `Outcome` is `#[non_exhaustive]`, so a shape this binary does not know
        // is possible in principle. Answering with its absence is honest;
        // guessing at its content would not be.
        //
        // This arm is correct and it is also where `Removed` hid: it answered
        // `unknown` for a known outcome, and §3.5 defines `unknown` as *a kind
        // this client has never seen*, so a conforming client reported version
        // skew that did not exist. Nothing distinguishes a correct wildcard from
        // one absorbing a known case except enumerating the variants against the
        // arms — which is what `every_outcome_this_build_knows_has_its_own_kind`
        // below does, and why it must gain a case whenever `Outcome` does.
        _ => body.push_str(r#"{"kind":"unknown"}"#),
    }
}

/// The access path, reported because a scan should be visible rather than
/// folklore — the same reason the embedded surface carries it.
const fn name_of(path: AccessPath) -> &'static str {
    path.name()
}

/// A failure, as the status that says what kind it was.
pub(crate) fn failure(error: &Error) -> Answer {
    let status = match error {
        // This node does not know who is asking: no credential against a closed
        // store, or one it refused. Both are answered the same way, because
        // telling them apart tells an attacker which half to keep guessing at.
        // A token whose account has since changed belongs here too, and for the
        // same reason it is not a 403: the holder was somebody, the store no
        // longer agrees, and what fixes it is signing in again.
        Error::NotSignedIn { .. }
        | Error::SignInRefused
        | Error::TicketStale
        // The caller is signed in and the second proof failed, which is still
        // "identify yourself" — a client acts on it by asking for the password
        // again, exactly as for the first.
        | Error::CurrentPasswordRefused => 401,
        // It declined to look. Not a 401, because a client told "wrong" retries
        // with a different password and one told "too many" must retry with the
        // same one later — and 429 is the status every client library already
        // backs off on.
        Error::SignInThrottled => 429,
        // It knows, and the answer is still no. A different thing entirely, and
        // a client that cannot tell retries a signin that will never help.
        //
        // `NotGranted` belongs here for exactly that reason and was reaching the
        // catch-all instead: a caller whose grants do not cover the table was
        // being told they had written the request wrongly, which is the one
        // thing they could not fix.
        // `NotTheWholeStore` belongs with these and not with `401`: the node
        // knows exactly who is asking, and signing in again will never help.
        // `CannotHandOut` is here for the same reason and not with the 400s: a
        // caller trying to grant past their own holdings wrote the statement
        // exactly right, and the refusal is about who they are.
        // These four were reaching the catch-all for the same reason
        // `NotGranted` did. A grant-governed user asking for a backup or trying
        // to declare structure wrote a statement this store understands
        // perfectly; an owner reaching a user outside their own tenancy, or
        // declaring somebody who would reach further than they do, likewise.
        // Every one of them is `CannotHandOut`'s case — the statement is right
        // and the refusal is about who is asking.
        Error::RoleForbids { .. }
        | Error::OutsideTenancy { .. }
        | Error::NotGranted { .. }
        | Error::NotTheWholeStore { .. }
        | Error::CannotHandOut { .. }
        | Error::GrantedUserCannotBackUp { .. }
        | Error::GrantedUserCannotDeclare { .. }
        | Error::NotYours { .. }
        | Error::WiderThanYou { .. } => 403,
        // The caller wrote it wrong, and no amount of changing the data helps.
        // A new password that is not one is a bad request rather than a
        // refusal: nothing about the caller's authority is in question.
        Error::PasswordEmpty { .. } => 400,
        Error::Script(_) => 400,
        // The caller wrote it right and the data says no. Retriable after a
        // change, which is the whole reason this is not a 400.
        //
        // The session raises its own two of these rather than wrapping a store
        // error, so they were answering 400 while meaning exactly what this arm
        // means: a `CREATE` over a record that exists succeeds once the record
        // goes, and a drop blocked by a dependency succeeds once the dependant
        // does. A client told `400` stops retrying, which is the one response
        // that never becomes right.
        Error::Store(_) | Error::RecordExists { .. } | Error::StillDepended { .. } => 409,
        // A substrate or decoding failure. Anything reaching here is a bug.
        //
        // A backup the writer could not write is a device speaking, not a
        // caller; an identity the store could not produce and a fold that
        // reached the evaluator are invariants of this build. Reported as 400
        // they read as user error and no alert ever sees them.
        Error::Encoding(_)
        | Error::BackupFailed { .. }
        | Error::IdentityUnavailable { .. }
        | Error::FoldOutsideAGroup { .. } => 500,
        // Everything else the session raises is about the script: an unselected
        // namespace, a wrong argument, a condition that is not a boolean.
        _ => 400,
    };
    let mut body = String::from(r#"{"error":"#);
    json::string(&mut body, &error.to_string());
    body.push('}');
    Answer::new(status, body)
}

#[cfg(test)]
mod corpus;

#[cfg(test)]
mod tests {
    use tessaridb::{Outcome, Value};

    use super::{encode, json};

    fn rendered(outcome: &Outcome) -> String {
        let mut body = String::new();
        encode(&mut body, outcome, &json::Names::new());
        body
    }

    #[test]
    fn a_conditional_deletes_count_reaches_the_caller() {
        // `done` would make an operator run a count before and after to learn
        // what their retention policy did. `unknown` — which is what this
        // answered — tells them their client is out of date instead.
        assert_eq!(
            rendered(&Outcome::Removed { count: 12_043 }),
            r#"{"kind":"removed","count":12043}"#
        );
    }

    #[test]
    fn every_outcome_this_build_knows_has_its_own_kind() {
        // The guard, and the reason this test exists rather than a review note:
        // the wildcard arm below `Removed` is *correct* and cannot be removed,
        // because `Outcome` is `#[non_exhaustive]`. Nothing tells a correct
        // wildcard apart from one swallowing a known outcome except listing the
        // variants and checking each renders as itself.
        //
        // `Outcome::Records` is not here because building a `Plan` by hand adds
        // a dozen lines of fixture; it is covered against a real node by
        // `tests/routes.rs`, which asserts `"kind":"records"`. A new variant
        // added to `Outcome` belongs in this list.
        for (outcome, expected) in [
            (Outcome::Done, "done"),
            (Outcome::Value(Value::Null), "value"),
            (Outcome::Keys(Vec::new()), "keys"),
            (Outcome::Removed { count: 0 }, "removed"),
        ] {
            let body = rendered(&outcome);
            assert!(
                body.contains(&format!(r#""kind":"{expected}""#)),
                "{outcome:?} rendered as {body}"
            );
            assert!(
                !body.contains(r#""kind":"unknown""#),
                "{outcome:?} rendered as unknown: {body}"
            );
        }
    }

    #[test]
    fn a_refusal_that_is_not_the_callers_fault_does_not_answer_400() {
        use tessari_ql::Span;
        use tessari_session::{Depended, Error};

        // The catch-all below the named arms answers `400` — "you wrote it
        // wrong" — and for most of what the session raises that is true. These
        // nine were reaching it while belonging to a row the protocol
        // specification already publishes (§5.2): a client branches on the
        // status, and `400` tells it to stop retrying and fix its request, which
        // is the one thing that never helps for any of these.
        let at = Span::new(0, 1);
        let user = || "someone".to_owned();
        let cases: Vec<(u16, Error)> = vec![
            // Authenticated, and the answer is still no. Signing in again never
            // helps, which is the whole reason 401 and 403 are kept apart.
            (
                403,
                Error::GrantedUserCannotBackUp {
                    user: user(),
                    span: at,
                },
            ),
            (
                403,
                Error::GrantedUserCannotDeclare {
                    user: user(),
                    span: at,
                },
            ),
            (
                403,
                Error::NotYours {
                    user: user(),
                    span: at,
                },
            ),
            (
                403,
                Error::WiderThanYou {
                    user: user(),
                    span: at,
                },
            ),
            // Written right, and the data says no. Retriable after a change —
            // the file's own definition of the 409 it already gives `Store`.
            (
                409,
                Error::RecordExists {
                    id: "users:1".to_owned(),
                    span: at,
                },
            ),
            (
                409,
                Error::StillDepended {
                    depended: Depended::DatabaseByTable,
                    name: "d".to_owned(),
                    count: 1,
                    first: "t".to_owned(),
                    span: at,
                },
            ),
            // A device or an invariant, not a caller. `BackupFailed` at 400 puts
            // a failed write behind "you asked wrongly", where no alert reads it.
            (
                500,
                Error::BackupFailed {
                    reason: "the device is full".to_owned(),
                },
            ),
            (
                500,
                Error::IdentityUnavailable {
                    reason: "exhausted",
                    span: at,
                },
            ),
            (500, Error::FoldOutsideAGroup { span: at }),
        ];
        for (expected, error) in cases {
            assert_eq!(
                super::failure(&error).status,
                expected,
                "{error} answered the wrong status"
            );
        }
    }
}
