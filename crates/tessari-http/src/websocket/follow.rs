//! What a browser sends to start following changes, and what it gets back.
//!
//! # Why the request carries a namespace and a database
//!
//! Every other route on this surface is one request that says everything about
//! itself. A socket is a session, and a feed has to be *inside* a database —
//! `tessaridb::feed` refuses one that is not, because a subscription to nothing
//! looks exactly like a quiet table. There is no `USE` before this message, so
//! this message carries what a `USE` would have said.
//!
//! # Why credentials may arrive here
//!
//! A browser cannot set headers on a `WebSocket`. `Authorization` works for
//! every other client and is read from the handshake when present, but for the
//! one client this route exists to serve it is not available, so the request may
//! carry a name and password instead. Recorded plainly rather than left to be
//! inferred: this is a credential inside a message body, which is acceptable
//! only because this store already states it has no TLS and belongs on a network
//! the operator protects.

use tessaridb::feed::Delivered;
use tessaridb::{Change, ChangeKind, Value, Visible, seen};

use crate::json::{self, Names};
use crate::request::Reader;

/// What a subscriber asked for, as it arrives on the socket.
#[derive(Debug)]
pub(crate) struct Asked {
    /// The namespace to follow changes in.
    pub(crate) namespace: String,
    /// The database within it.
    pub(crate) database: String,
    /// The first position to read, inclusive. `0` is everything the log holds.
    pub(crate) from: u64,
    /// One table, or every table the session may read.
    pub(crate) table: Option<String>,
    /// A credential, when the handshake could not carry one.
    pub(crate) credentials: Option<(String, String)>,
    /// A token from an earlier sign-in, when the handshake could not carry one.
    ///
    /// Preferred over `credentials` by whoever reads this, and it is what a
    /// browser should send: a `WebSocket` cannot carry a header, so whatever
    /// authenticates it goes in the message — and a token that expires and can
    /// be revoked is a better thing to put there than a password that does
    /// neither.
    pub(crate) token: Option<String>,
}

/// Read a follow request.
///
/// # Errors
///
/// Returns what is wrong with it, in the words the subscriber needs to fix it.
pub(crate) fn read(body: &str) -> Result<Asked, String> {
    let mut at = Reader::new(body);
    at.space();
    at.expect('{')?;
    let mut namespace = None;
    let mut database = None;
    let mut from = 0;
    let mut table = None;
    let mut user = None;
    let mut password = None;
    let mut token = None;
    at.space();
    if !at.eat('}') {
        loop {
            at.space();
            let key = at.string()?;
            at.space();
            at.expect(':')?;
            at.space();
            match key.as_str() {
                "namespace" => namespace = Some(at.string()?),
                "database" => database = Some(at.string()?),
                "from" => from = at.number()?,
                "table" => table = Some(at.string()?),
                "user" => user = Some(at.string()?),
                "password" => password = Some(at.string()?),
                "token" => token = Some(at.string()?),
                other => return Err(format!("a follow request has no {other:?} field")),
            }
            at.space();
            if at.eat(',') {
                continue;
            }
            at.expect('}')?;
            break;
        }
    }
    at.space();
    if !at.done() {
        return Err("the request ends before the message does".to_owned());
    }
    Ok(Asked {
        namespace: namespace.ok_or_else(|| "a follow request needs a `namespace`".to_owned())?,
        database: database.ok_or_else(|| "a follow request needs a `database`".to_owned())?,
        from,
        table,
        // Both or neither: a name without a password is a request that would
        // sign in as somebody with no proof, and refusing it here is clearer
        // than letting the sign-in fail for a reason nobody wrote down.
        credentials: match (user, password) {
            (Some(name), Some(secret)) => Some((name, secret)),
            _ => None,
        },
        token,
    })
}

/// One change, as the object a browser parses.
///
/// The seventeen-into-six compromise ADR-0016 recorded for this surface — it
/// was fifteen types when that decision was written, and the ADR keeps its own
/// number because a dated record should — applies here exactly as it does to a
/// query answer: the same encoder, so a value does not mean one thing in a
/// reply and another in a feed.
pub(crate) fn encode(change: &Change, table: &str, allowed: &Visible, names: &Names) -> String {
    let mut out = String::from(r#"{"sequence":"#);
    out.push_str(&change.sequence.get().to_string());
    out.push_str(r#","table":"#);
    json::string(&mut out, table);
    out.push_str(r#","id":"#);
    json::string(&mut out, &change.id.to_string());
    match &change.kind {
        ChangeKind::Written(held) => {
            out.push_str(r#","became":"written","value":"#);
            let shown: Value = seen(held.clone(), allowed);
            json::write(&mut out, &shown, names);
        }
        // A removal carries no value, so there is nothing in it to hide — and
        // *that* a record went is what the table grant already decided this
        // subscriber may know.
        ChangeKind::Removed => out.push_str(r#","became":"removed""#),
    }
    out.push('}');
    out
}

/// What a refusal looks like, so a client can tell one from a change.
pub(crate) fn refusal(reason: &str) -> String {
    let mut out = String::from(r#"{"error":"#);
    json::string(&mut out, reason);
    out.push('}');
    out
}

/// A sink that could not write is a connection that has gone.
pub(crate) const GONE: Delivered = false;

#[cfg(test)]
mod tests {
    use super::read;

    #[test]
    fn a_request_names_where_to_follow_and_from_when() {
        let asked = read(r#"{"namespace":"n","database":"d","from":12,"table":"users"}"#)
            .expect("a well-formed request");
        assert_eq!(asked.namespace, "n");
        assert_eq!(asked.database, "d");
        assert_eq!(asked.from, 12);
        assert_eq!(asked.table.as_deref(), Some("users"));
        assert!(
            asked.credentials.is_none(),
            "a request with no user must not arrive carrying one"
        );
    }

    #[test]
    fn from_defaults_to_the_whole_log_and_a_table_is_optional() {
        let asked = read(r#"{"namespace":"n","database":"d"}"#).expect("a well-formed request");
        assert_eq!(asked.from, 0, "an unstated position must mean everything");
        assert!(asked.table.is_none(), "an unstated table must mean all");
    }

    #[test]
    fn a_name_without_a_password_is_not_a_credential() {
        let asked = read(r#"{"namespace":"n","database":"d","user":"root"}"#)
            .expect("a well-formed request");
        assert!(
            asked.credentials.is_none(),
            "a name with no password would sign in without proof"
        );
    }

    #[test]
    fn what_is_missing_is_named_rather_than_guessed() {
        let reason = read(r#"{"database":"d"}"#).expect_err("a request with no namespace");
        assert!(
            reason.contains("namespace"),
            "the refusal must say which field is missing, said: {reason}"
        );
    }

    #[test]
    fn a_field_this_request_does_not_have_is_refused() {
        let reason = read(r#"{"namespace":"n","database":"d","limit":5}"#)
            .expect_err("a request with an unknown field");
        assert!(
            reason.contains("limit"),
            "the refusal must name the field it did not recognise, said: {reason}"
        );
    }
}
