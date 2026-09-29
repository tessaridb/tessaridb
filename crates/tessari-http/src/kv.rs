//! `/kv/{namespace}/{database}/{space}/{op}/{key…}` — a space over HTTP (ADR-0090).
//!
//! # A surface over the space's own statements, like every route here
//!
//! Each operation is one statement a caller could have sent to `POST /script`,
//! run through the caller's own session, so a grant, a tenancy and a refusal are
//! the ones a script would meet. What the route adds is that the one operation
//! easy to get wrong — handing a lock back — is spelled correctly once, here.
//!
//! # What comes from where
//!
//! The namespace, the database and the space are **names** and are interpolated,
//! so each is checked to be an ordinary identifier first — the object routes'
//! rule. The key is the percent-decoded rest of the path and is **bound**, as is
//! every value, duration and holder: nothing a caller sends can become syntax.
//! The one number formatted into the text is a listing's `limit`, which the
//! grammar takes only as a literal and which is parsed to an integer first.
//!
//! The operation sits *before* the key, so a key containing `/incr` is a key.

mod ops;

use tessaridb::{Db, Value};

use crate::basic::Presented;
use crate::object::{decoded, is_identifier};
use crate::respond::{Answer, failure, session_for};
use crate::tokens::Tokens;

pub(crate) use ops::Request;

/// What a key-value request named.
pub(crate) struct Aimed<'a> {
    namespace: &'a str,
    database: &'a str,
    space: &'a str,
    /// The operation segment, or `None` for a listing of the space.
    op: Option<&'a str>,
    /// The key, decoded, when the path names one.
    key: Option<String>,
    query: &'a str,
}

/// Read `/kv/{namespace}/{database}/{space}[/{op}[/{key…}]]`, or `None` when the
/// URL is not a key-value route.
pub(crate) fn target(url: &str) -> Option<Aimed<'_>> {
    let rest = url.strip_prefix("/kv/")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut parts = path.splitn(5, '/');
    Some(Aimed {
        namespace: parts.next()?,
        database: parts.next()?,
        space: parts.next()?,
        op: parts.next(),
        key: parts.next().map(decoded),
        query,
    })
}

impl Aimed<'_> {
    /// The decoded value of query parameter `name`, if it was given.
    pub(crate) fn param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| decoded(value))
        })
    }
}

/// Answer one key-value request.
///
/// Everything a request can get wrong by itself — a name, an operation, a
/// duration, a body — is refused before a session is opened, so a malformed
/// request costs nothing and changes nothing.
pub(crate) fn answer(
    db: &Db,
    method: &axum::http::Method,
    aimed: &Aimed<'_>,
    body: &str,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    if ![aimed.namespace, aimed.database, aimed.space]
        .iter()
        .all(|name| is_identifier(name))
    {
        return Answer::bad_request(
            "a namespace, a database and a space are names: letters, digits and underscores",
        );
    }
    let request = match Request::read(method, aimed, body) {
        Ok(request) => request,
        Err(refused) => return refused,
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
    // Asked after the `USE`, so only a caller who may reach the database learns
    // what it holds; not there and not a space are one answer.
    match db.is_space(aimed.namespace, aimed.database, aimed.space) {
        Ok(true) => {}
        Ok(false) => {
            return Answer::new(
                404,
                format!(
                    r#"{{"error":"no space named {} in {}.{}"}}"#,
                    aimed.space, aimed.namespace, aimed.database
                ),
            );
        }
        Err(error) => return failure(&error),
    }
    request.run(&mut session, db, aimed.space)
}

/// A duration a caller gave, when it is one and is in the future.
///
/// Refused rather than passed on when it is not positive: `EXPIRE` with a past
/// or zero duration **removes** the key, which is not what a caller who typed
/// `-5s` into an expiry asked a route to do.
fn positive_duration(text: &str) -> Option<Value> {
    match tessaridb::value_of(text) {
        Ok(Value::Duration(held))
            if held.seconds() > 0 || (held.seconds() == 0 && held.nanos() > 0) =>
        {
            Some(Value::Duration(held))
        }
        _ => None,
    }
}
