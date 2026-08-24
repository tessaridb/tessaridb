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

use tessari::{AccessPath, Db, Error, Outcome};
use tessari_serve::{Census, Stopping};

use crate::basic::Credentials;
use crate::json;

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
        Err(error) => return failure(&tessari::Error::from(error)),
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
pub(crate) fn metrics(db: &Db, census: Option<&Census>, mine: &Stopping) -> Answer {
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
    }

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
/// A session, signed in when a credential was presented.
///
/// Shared by the script route and the object routes rather than written twice,
/// because "who is asking" must be one answer: two sign-in paths is two places a
/// refusal can be forgotten.
pub(crate) fn session_for<'a>(
    db: &'a Db,
    credentials: Option<&Credentials>,
) -> Result<tessari::Session<'a>, Answer> {
    let mut session = db.session();
    if let Some(presented) = credentials
        && let Err(error) = session.sign_in(&presented.name, &presented.password)
    {
        return Err(failure(&error));
    }
    Ok(session)
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
pub(crate) fn backup(db: &Db, query: Option<&str>, credentials: Option<&Credentials>) -> Answer {
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
    let mut session = match session_for(db, credentials) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let script = from.map_or_else(
        || "BACKUP;".to_owned(),
        |held| format!("BACKUP FROM {held};"),
    );
    match session.run(&script) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Value(tessari::Value::Bytes(bytes))) => {
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

/// A bucket's listing, as the records it answered with.
pub(crate) fn listing(db: &Db, outcomes: &[Outcome]) -> Answer {
    let Some(outcome) = outcomes.last() else {
        return Answer::new(200, r#"{"files":[]}"#.to_owned());
    };
    let referenced = match outcome {
        Outcome::Records { records, .. } => records.clone(),
        _ => Vec::new(),
    };
    let names = db.names_in(&referenced).unwrap_or_default();
    let mut body = String::from(r#"{"files":["#);
    encode(&mut body, outcome, &names);
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
    credentials: Option<&Credentials>,
) -> Answer {
    let mut session = match session_for(db, credentials) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let mut given = tessari::Parameters::new();
    for (name, value) in written {
        match tessari::value_of(value) {
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
            let referenced: Vec<(tessari::RecordId, tessari::Value)> = outcomes
                .iter()
                .flat_map(|outcome| match outcome {
                    Outcome::Records { records, .. } => records.clone(),
                    Outcome::Value(held) => {
                        vec![(tessari::RecordId::Int(0), held.clone())]
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
pub(crate) fn failure(error: &Error) -> Answer {
    let status = match error {
        // This node does not know who is asking: no credential against a closed
        // store, or one it refused. Both are answered the same way, because
        // telling them apart tells an attacker which half to keep guessing at.
        Error::NotSignedIn { .. } | Error::SignInRefused => 401,
        // It knows, and the answer is still no. A different thing entirely, and
        // a client that cannot tell retries a signin that will never help.
        //
        // `NotGranted` belongs here for exactly that reason and was reaching the
        // catch-all instead: a caller whose grants do not cover the table was
        // being told they had written the request wrongly, which is the one
        // thing they could not fix.
        Error::RoleForbids { .. } | Error::OutsideTenancy { .. } | Error::NotGranted { .. } => 403,
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
