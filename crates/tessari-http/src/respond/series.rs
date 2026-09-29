//! `POST /series/{namespace}/{database}/{series}` — a batch of events appended
//! in one transaction (G044 C12).
//!
//! # A surface over `CREATE`, like every route here
//!
//! The batch runs as `BEGIN; CREATE s = $e0; …; COMMIT;` through the caller's
//! own session, so a grant, a tenancy, the series' event-time checks and its
//! rollups behave exactly as they do for a script. A refusal anywhere in the
//! batch ends the script before its `COMMIT`, and the session's open
//! transaction is discarded with it: the batch lands whole or not at all.
//!
//! # What comes from where
//!
//! The three names come from the URL and are interpolated, so each is checked
//! to be an ordinary identifier first — the object routes' rule. The events are
//! the body: ONE TessariQL value, an array of objects written as literals, read
//! in isolation and bound. Nothing in it can become syntax, and nothing in it
//! is evaluated.
//!
//! # What "not a series" tells a caller
//!
//! The kind is asked after the session has taken the `USE`, so only a caller who
//! may reach that database learns whether a series of that name is there; not
//! there and not a series are one answer, `404`.

use tessaridb::{Db, Parameters, Value};

use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::object::is_identifier;
use crate::tokens::Tokens;

/// What an append request named.
pub(crate) struct Aimed<'a> {
    namespace: &'a str,
    database: &'a str,
    series: &'a str,
}

/// Read `/series/{namespace}/{database}/{series}`, or `None` when the URL names
/// fewer than three.
pub(crate) fn target(url: &str) -> Option<Aimed<'_>> {
    let rest = url.strip_prefix("/series/")?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.splitn(3, '/');
    Some(Aimed {
        namespace: parts.next()?,
        database: parts.next()?,
        series: parts.next()?,
    })
}

/// Append the batch in `body` to the series `aimed` names.
pub(crate) fn append(
    db: &Db,
    aimed: &Aimed<'_>,
    body: &str,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    if ![aimed.namespace, aimed.database, aimed.series]
        .iter()
        .all(|name| is_identifier(name))
    {
        return Answer::bad_request(
            "a namespace, a database and a series are names: letters, digits and underscores",
        );
    }
    let Some(events) = events(body) else {
        return Answer::bad_request(
            "the body is one TessariQL array of objects, every value in it a literal",
        );
    };
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    if let Err(error) = session.run(&format!(
        "USE NAMESPACE {}; USE DATABASE {};",
        aimed.namespace, aimed.database
    )) {
        return failure(&error);
    }
    match db.is_series(aimed.namespace, aimed.database, aimed.series) {
        Ok(true) => {}
        Ok(false) => {
            return Answer::new(
                404,
                format!(
                    r#"{{"error":"no series named {} in {}.{}"}}"#,
                    aimed.series, aimed.namespace, aimed.database
                ),
            );
        }
        Err(error) => return failure(&error),
    }
    let count = events.len();
    let mut script = String::from("BEGIN;");
    let mut given = Parameters::new();
    for (position, event) in events.into_iter().enumerate() {
        script.push_str(&format!(" CREATE {} = $e{position};", aimed.series));
        given.insert(format!("e{position}"), event);
    }
    script.push_str(" COMMIT;");
    match session.run_with(&script, &given) {
        Ok(_) => Answer::new(200, format!(r#"{{"appended":{count}}}"#)),
        Err(error) => failure(&error),
    }
}

/// The body's events, when it is an array of objects.
fn events(body: &str) -> Option<Vec<Value>> {
    let Ok(Value::Array(events)) = tessaridb::value_of(body.trim()) else {
        return None;
    };
    events
        .iter()
        .all(|event| matches!(event, Value::Object(_)))
        .then_some(events)
}
