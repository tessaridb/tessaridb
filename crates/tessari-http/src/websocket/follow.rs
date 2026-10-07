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
//! inferred: this is a credential inside a message body, protected by exactly
//! what protects an `Authorization` header — the node's TLS when it was given a
//! certificate, and the operator's network when it was not (ADR-0108 D4).

use tessaridb::{Change, ChangeKind, Value, Visible, seen};

use crate::json::{self, Names};
use crate::request::Reader;

/// What a subscriber asked for, as it arrives on the socket.
pub(crate) struct Asked {
    /// The namespace to follow changes in.
    pub(crate) namespace: String,
    /// The database within it.
    pub(crate) database: String,
    /// The first position to read, inclusive. `0` is everything the log holds.
    pub(crate) from: u64,
    /// One table, or every table the session may read.
    pub(crate) table: Option<String>,
    /// On a feed over a split table, the `cursor` the last change handled
    /// carried.
    pub(crate) cursor: Option<String>,
    /// A condition narrowing the feed, and its parameters as TessariQL values
    /// read the way `/script` reads them (ADR-0122 Part B).
    pub(crate) condition: Option<(String, tessaridb::Parameters)>,
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

/// Wiped when the request is done with, as [`crate::basic::Credentials`] is.
impl Drop for Asked {
    fn drop(&mut self) {
        if let Some((_, password)) = &mut self.credentials {
            zeroize::Zeroize::zeroize(password);
        }
    }
}

/// Written by hand, because the derived one prints the password.
impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Asked")
            .field("namespace", &self.namespace)
            .field("database", &self.database)
            .field("from", &self.from)
            .field("table", &self.table)
            .field("cursor", &self.cursor)
            .field("as", &self.credentials.as_ref().map(|(name, _)| name))
            .field("token", &self.token.as_ref().map(|_| ".."))
            .finish()
    }
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
    let mut cursor = None;
    let mut condition = None;
    let mut written = std::collections::BTreeMap::new();
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
                "cursor" => cursor = Some(at.string()?),
                "condition" => condition = Some(at.string()?),
                "parameters" => written = at.strings()?,
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
    let mut parameters = tessaridb::Parameters::new();
    for (name, value) in &written {
        let held =
            tessaridb::value_of(value).map_err(|reason| format!("parameter {name}: {reason}"))?;
        parameters.insert(name.clone(), held);
    }
    if condition.is_none() && !parameters.is_empty() {
        return Err("`parameters` are bound into a `condition`, and there is none".to_owned());
    }
    Ok(Asked {
        namespace: namespace.ok_or_else(|| "a follow request needs a `namespace`".to_owned())?,
        database: database.ok_or_else(|| "a follow request needs a `database`".to_owned())?,
        from,
        table,
        cursor,
        condition: condition.map(|text| (text, parameters)),
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
///
/// A change of a feed over a split table also carries the `cursor` to resume
/// after it, since its logs count separately and no one `sequence` says where
/// the feed was.
pub(crate) fn encode(
    change: &Change,
    table: &str,
    allowed: &Visible,
    names: &Names,
    cursor: Option<&str>,
) -> String {
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
    if let Some(cursor) = cursor {
        out.push_str(r#","cursor":"#);
        json::string(&mut out, cursor);
    }
    out.push('}');
    out
}

/// What a refusal looks like, so a client can tell one from a change.
/// How far a narrowed feed read past what it sent (ADR-0122 B3).
pub(crate) fn progress(sequence: u64, cursor: Option<&str>) -> String {
    let mut out = String::from(r#"{"progress":"#);
    out.push_str(&sequence.to_string());
    if let Some(cursor) = cursor {
        out.push_str(r#","cursor":"#);
        json::string(&mut out, cursor);
    }
    out.push('}');
    out
}

pub(crate) fn refusal(reason: &str) -> String {
    let mut out = String::from(r#"{"error":"#);
    json::string(&mut out, reason);
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::read;

    #[test]
    fn printing_a_request_never_shows_its_password() {
        let asked = read(
            r#"{"namespace":"n","database":"d","from":0,"user":"ada","password":"correct horse"}"#,
        )
        .expect("a well-formed request");
        let printed = format!("{asked:?}");
        assert!(printed.contains("ada"), "{printed}");
        assert!(!printed.contains("correct horse"), "{printed}");
    }

    #[test]
    fn a_request_names_where_to_follow_and_from_when() {
        let asked = read(r#"{"namespace":"n","database":"d","from":12,"table":"users"}"#)
            .expect("a well-formed request");
        assert_eq!(asked.namespace, "n");
        assert_eq!(asked.database, "d");
        assert_eq!(asked.from, 12);
        assert_eq!(asked.table.as_deref(), Some("users"));
        assert!(
            asked.cursor.is_none(),
            "a request with no cursor must not carry one"
        );
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
    fn a_cursor_travels_as_it_was_given() {
        let asked = read(r#"{"namespace":"n","database":"d","cursor":"d=4,9.1=2"}"#)
            .expect("a well-formed request");
        assert_eq!(asked.cursor.as_deref(), Some("d=4,9.1=2"));
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

    #[test]
    fn a_request_names_a_condition_with_values_read_the_way_a_script_reads_them() {
        let asked = read(
            r#"{"namespace":"n","database":"d","table":"msgs","condition":"chat = $chat","parameters":{"chat":"'a'"}}"#,
        )
        .expect("a well-formed request");
        let (text, parameters) = asked.condition.clone().expect("the condition");
        assert_eq!(text, "chat = $chat");
        assert_eq!(parameters.get("chat"), Some(&tessaridb::Value::from("a")));
        // Values with nothing to bind into are a mistake, said as one.
        assert!(read(r#"{"namespace":"n","database":"d","parameters":{"x":"1"}}"#).is_err());
        assert_eq!(
            super::progress(9, Some("1.1:d=10")),
            r#"{"progress":9,"cursor":"1.1:d=10"}"#
        );
        assert_eq!(super::progress(9, None), r#"{"progress":9}"#);
    }
}
