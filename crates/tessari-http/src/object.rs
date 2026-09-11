//! The object routes: put a file, get one, delete one, list a bucket.
//!
//! # This is a surface over the statements, not a second path
//!
//! ADR-0011 §6. Every route below runs a TessariQL statement through an ordinary
//! session, so an identity, a grant, a tenancy and a refusal behave identically
//! whether a caller speaks the language or speaks HTTP. The alternative — a
//! route reaching the store directly — is a second permission model, and the
//! kind that is discovered rather than designed.
//!
//! # What comes from the URL, and what that costs
//!
//! `/files/{namespace}/{database}/{bucket}/{path…}`
//!
//! The path is a **value**, so it travels as a parameter and cannot become part
//! of the statement — `/files/prod/library/media/'; DROP TABLE users; --` is a
//! file with an unusual name.
//!
//! The first three are **names**, and a parameter may never supply a name. So
//! they are interpolated into the statement, and that is safe only because each
//! is checked to be an ordinary identifier first: letters, digits and
//! underscores, nothing else, nothing empty. A segment that is not one is a 400
//! before any statement exists — which is the check being *in front of* the
//! interpolation rather than trusted to be somewhere.

use tessaridb::{Db, Error, Outcome, Parameters, Value};

use crate::basic::Presented;
use crate::respond::{Answer, failure, session_for};
use crate::tokens::Tokens;

/// What a request named.
pub(crate) struct Target<'a> {
    namespace: &'a str,
    database: &'a str,
    bucket: &'a str,
    /// The file's path, or `None` when the request named a bucket and no file.
    path: Option<String>,
}

/// Read `/files/{namespace}/{database}/{bucket}[/{path…}]`.
///
/// Returns `None` when the URL is not an object route at all, so the caller can
/// go on to answer 404 the way it does for anything else.
pub(crate) fn target(url: &str) -> Option<Target<'_>> {
    let rest = url.strip_prefix("/files/")?;
    // The query string belongs to nobody here; a file's name is the path.
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.splitn(4, '/');
    let namespace = parts.next()?;
    let database = parts.next()?;
    let bucket = parts.next()?;
    // The remainder is the file's path, with its separators intact: a bucket is
    // flat, so `/photos/a.png` is a name that happens to contain slashes rather
    // than a directory somebody has to create.
    let path = parts.next().map(|held| format!("/{}", decoded(held)));
    Some(Target {
        namespace,
        database,
        bucket,
        path,
    })
}

impl Target<'_> {
    /// Whether every name in this URL is one a statement may carry.
    fn named_properly(&self) -> bool {
        [self.namespace, self.database, self.bucket]
            .iter()
            .all(|held| is_identifier(held))
    }

    /// The `USE` that puts a session where this request is aimed.
    fn tenancy(&self) -> String {
        format!(
            "USE NAMESPACE {}; USE DATABASE {};",
            self.namespace, self.database
        )
    }
}

/// `PUT /files/…/{path}` — write a file.
/// A refusal from one of these four routes, with "there is no bucket there"
/// answered `404` rather than `400` (Q-515).
///
/// # Why the mapping is here and not in `failure`
///
/// [`Error::Unknown`] carries an `entity` and is raised for a table, a namespace,
/// a user and an index as readily as for a bucket, and [`failure`] serves every
/// route on this surface — including `POST /script`. Moving either error inside
/// that shared match would change all of them at once, and on `/script` a `404`
/// would be a claim about the URI, which is `/script` and is fine.
///
/// These four routes are the ones where the URI **names** the bucket, so they are
/// the ones that can say it is not there. A caller who asked for a bucket that is
/// not there did not write a malformed request, and `400` sends them to fix the
/// one thing that was not wrong — the same correction [`failure`] already makes
/// for `NotGranted` and for `RecordExists`.
///
/// # Three shapes, not two, and `"table"` is not a widening
///
/// "There is no bucket here" reaches this function under three labels, because
/// two different resolvers get there first. `INFO FOR BUCKET` funnels all of its
/// failures through one constructor and so reports `entity: "bucket"` for an
/// undeclared name **and** for one declared as something else; `Store::bucket`
/// resolves before it checks the kind, so an undeclared name on the three file
/// routes never reaches `NotABucket` at all — `Context::resolve_table` has
/// already raised `entity: "table"`.
///
/// Matching `"table"` here catches that third shape and widens nothing, because
/// the only table name any of these four statements can carry **is** the bucket
/// segment: the file's path travels as a parameter, and the namespace and
/// database are their own entities. On this surface, `no table named …` can only
/// ever mean the bucket segment named nothing.
///
/// The body is [`failure`]'s own, unchanged, and it differs by route — the
/// listing says *"no bucket named …"* whichever way it failed, while a file route
/// says *"… is not a bucket"* for a name that is declared as something else. So
/// the status says **it is not here** and the sentence says which; a client is
/// told to surface it rather than parse it.
fn refused(error: &Error) -> Answer {
    let mut answer = failure(error);
    if matches!(
        error,
        Error::Unknown {
            entity: "bucket" | "table",
            ..
        } | Error::NotABucket { .. }
    ) {
        answer.status = 404;
    }
    answer
}

pub(crate) fn put(
    db: &Db,
    target: &Target<'_>,
    bytes: Vec<u8>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let Some(path) = target.path.clone() else {
        return Answer::bad_request("a put needs a file's path");
    };
    if !target.named_properly() {
        return Answer::bad_request("a namespace, database and bucket are ordinary names");
    }
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let mut given = Parameters::new();
    given.insert("path".to_owned(), Value::String(path));
    given.insert("held".to_owned(), Value::Bytes(bytes));
    let script = format!("{} PUT {}:$path = $held;", target.tenancy(), target.bucket);
    match session.run_with(&script, &given) {
        Ok(_) => Answer::new(201, r#"{"written":true}"#.to_owned()),
        Err(error) => refused(&error),
    }
}

/// `GET /files/…/{path}` — a file's bytes, or the bucket's listing.
pub(crate) fn get(db: &Db, target: &Target<'_>, tokens: &Tokens, presented: &Presented) -> Answer {
    if !target.named_properly() {
        return Answer::bad_request("a namespace, database and bucket are ordinary names");
    }
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let Some(path) = target.path.clone() else {
        // No path: the request named a bucket, and listing one is a query.
        //
        // `INFO FOR BUCKET` runs first because `SELECT` alone cannot tell the
        // two apart: it succeeds against any table, so listing a name declared
        // with `DEFINE TABLE` answered `200` with an empty listing and a caller
        // concluded the bucket was empty rather than absent (Q-261). `PUT` and
        // `READ` refuse it on their own, because they are file statements and
        // resolve the bucket themselves. `DELETE` is not one and did not — it
        // ran a plain record delete and answered `204` against any table at all
        // — so it now asks the same question this branch does, which is what
        // makes all four routes agree at last (Q-515, W206).
        //
        // It is asked as a **statement**, through the same session, rather than
        // by reaching into the catalog from here — a route that reaches the
        // store directly is a second permission model, "the kind that is
        // discovered rather than designed" (ADR-0011 §6).
        let script = format!(
            "{} INFO FOR BUCKET {}; SELECT * FROM {};",
            target.tenancy(),
            target.bucket,
            target.bucket,
        );
        return match session.run_with(&script, &Parameters::new()) {
            Ok(outcomes) => crate::respond::listing(&outcomes),
            Err(error) => refused(&error),
        };
    };
    let mut given = Parameters::new();
    given.insert("path".to_owned(), Value::String(path));
    let script = format!("{} READ {}:$path;", target.tenancy(), target.bucket);
    match session.run_with(&script, &given) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Value(Value::Bytes(bytes))) => Answer::octets(200, bytes.clone()),
            // `READ` answers `NONE` where there is no file, which is the store
            // saying "nothing there" rather than failing — and 404 is how HTTP
            // says the same thing.
            _ => Answer::new(404, r#"{"error":"no such file"}"#.to_owned()),
        },
        Err(error) => refused(&error),
    }
}

/// `DELETE /files/…/{path}` — remove a file and its bytes.
pub(crate) fn delete(
    db: &Db,
    target: &Target<'_>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let Some(path) = target.path.clone() else {
        return Answer::bad_request("a delete needs a file's path");
    };
    if !target.named_properly() {
        return Answer::bad_request("a namespace, database and bucket are ordinary names");
    }
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let mut given = Parameters::new();
    given.insert("path".to_owned(), Value::String(path));
    // `INFO FOR BUCKET` first, and this route is the reason the sentence above
    // about the other three refusing was not true of it.
    //
    // `PUT` and `READ` are file statements and resolve the bucket themselves, so
    // they refuse an ordinary table on their own. A delete is **not** a file
    // statement — there is no `DELETE FILE` — so this route was sending a plain
    // record delete, which asks nothing about bucket-ness. `DELETE
    // /files/ns/db/<a table>/<anything>` therefore answered `204`, reporting a
    // file removed from a bucket that does not exist, and would have removed a
    // real record from a real table had one carried that id. One route of four
    // disagreeing about what a bucket is, which is Q-261 again on the member of
    // the set nobody re-checked.
    //
    // Asked as a statement through the same session rather than by reaching into
    // the catalog from here, for the reason the listing branch above gives:
    // a route that reaches the store directly is a second permission model.
    let script = format!(
        "{} INFO FOR BUCKET {}; DELETE {}:$path;",
        target.tenancy(),
        target.bucket,
        target.bucket,
    );
    match session.run_with(&script, &given) {
        Ok(_) => Answer::new(204, String::new()),
        Err(error) => refused(&error),
    }
}

/// Whether a URL segment is a name a statement may carry.
///
/// Deliberately narrower than what the lexer accepts: this is the guard in front
/// of an interpolation, and a guard that reasons about what the lexer would do
/// is a guard that has to be re-checked every time the lexer changes.
pub(crate) fn is_identifier(held: &str) -> bool {
    !held.is_empty()
        && held
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Percent-decoding, because a file's path may hold a space or a slash.
///
/// Bytes rather than characters: a percent escape names a byte, and a path is
/// text only once the escapes are resolved. An escape that is not two hex digits
/// is left as written — a caller who meant a literal `%` gets one, which is
/// friendlier than refusing a name that is perfectly storable.
fn decoded(held: &str) -> String {
    let raw = held.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        let byte = raw.get(at).copied().unwrap_or(b'%');
        if byte == b'%'
            && let (Some(high), Some(low)) =
                (raw.get(at.saturating_add(1)), raw.get(at.saturating_add(2)))
            && let (Some(high), Some(low)) = (hex(*high), hex(*low))
        {
            out.push(high.saturating_mul(16).saturating_add(low));
            at = at.saturating_add(3);
            continue;
        }
        out.push(byte);
        at = at.saturating_add(1);
    }
    String::from_utf8(out).unwrap_or_else(|_| held.to_owned())
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.saturating_sub(b'0')),
        b'a'..=b'f' => Some(byte.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(byte.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{decoded, is_identifier, target};

    #[test]
    fn a_url_names_a_tenancy_a_bucket_and_a_path() {
        let held = target("/files/prod/library/media/photos/a%20b.png").unwrap();
        assert_eq!(held.namespace, "prod");
        assert_eq!(held.database, "library");
        assert_eq!(held.bucket, "media");
        assert_eq!(held.path.as_deref(), Some("/photos/a b.png"));
    }

    #[test]
    fn a_url_without_a_path_names_a_bucket() {
        let held = target("/files/prod/library/media").unwrap();
        assert!(held.path.is_none());
    }

    #[test]
    fn anything_else_is_not_an_object_route() {
        assert!(target("/script").is_none());
        assert!(target("/files/prod").is_none());
    }

    #[test]
    fn a_name_that_could_carry_syntax_is_not_a_name() {
        // The guard in front of the interpolation. Each of these would be a
        // statement rather than a name if it were let through.
        assert!(is_identifier("media"));
        assert!(is_identifier("media_2"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("media;DROP"));
        assert!(!is_identifier("media table"));
        assert!(!is_identifier("media'"));
    }

    #[test]
    fn a_percent_escape_that_is_not_one_is_left_alone() {
        assert_eq!(decoded("100%"), "100%");
        assert_eq!(decoded("a%zzb"), "a%zzb");
        assert_eq!(decoded("a%2Fb"), "a/b");
    }
}
