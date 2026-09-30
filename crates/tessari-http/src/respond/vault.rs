//! `GET /vault`, `POST /vault/unseal`, `POST /vault/seal` and
//! `POST /vault/passphrase` (ADR-0092 D2, D3) — and the same four under
//! `/vault/{namespace}/{database}/{vault}` for one vault carrying its own
//! passphrase (ADR-0093 D6).
//!
//! A surface over [`tessaridb::Session::vault`] rather than a second
//! implementation: who may unseal, the throttle and the answer are the
//! statement's own. What the route adds is only the transport — the passphrase
//! arrives as a body of its own, which is not a script and carries no grammar,
//! the shape `POST /password` already has. The body is never logged, echoed or
//! quoted in a refusal.

use tessaridb::{Db, VaultAct, VaultTarget};

use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::body;
use crate::incoming::Incoming;
use crate::json;
use crate::object::is_identifier;
use crate::request;
use crate::tokens::Tokens;
use axum::http::Method;

/// Carry out `act` on `target` as the caller and answer with the seal status
/// as JSON.
pub(crate) fn answer(
    db: &Db,
    target: VaultTarget<'_>,
    act: VaultAct<'_>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(refused) => return refused,
    };
    match session.vault(target, act) {
        Ok(status) => {
            let mut body = String::new();
            json::write(&mut body, &status, &json::Names::new());
            Answer::new(200, body)
        }
        Err(error) => failure(&error),
    }
}

/// `/vault/{namespace}/{database}/{vault}[/unseal|/seal|/passphrase]`.
pub(crate) fn one_vault(
    db: &Db,
    method: Method,
    url: &str,
    request: &mut Incoming,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let rest = url.strip_prefix("/vault/").unwrap_or_default();
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.splitn(4, '/');
    let (Some(namespace), Some(database), Some(vault)) = (parts.next(), parts.next(), parts.next())
    else {
        return Answer::new(404, r#"{"error":"no such route"}"#.to_owned());
    };
    let act = parts.next();
    if ![namespace, database, vault]
        .iter()
        .all(|name| is_identifier(name))
    {
        return Answer::bad_request(
            "a namespace, a database and a vault are names: letters, digits and underscores",
        );
    }
    let target = VaultTarget::Vault {
        namespace,
        database,
        vault,
    };
    match (method, act) {
        (Method::GET, None) => answer(db, target, VaultAct::Status, tokens, presented),
        (Method::POST, Some("seal")) => answer(db, target, VaultAct::Seal, tokens, presented),
        (Method::POST, Some("unseal")) => match body::text(request) {
            Ok(body) => answer(
                db,
                target,
                VaultAct::Unseal {
                    passphrase: body.trim_end_matches('\n'),
                },
                tokens,
                presented,
            ),
            Err(refused) => refused,
        },
        (Method::POST, Some("passphrase")) => match body::text(request) {
            Ok(body) => match request::passphrases(&body) {
                Ok((current, new)) => answer(
                    db,
                    target,
                    VaultAct::Change {
                        current: &current,
                        new: &new,
                    },
                    tokens,
                    presented,
                ),
                Err(shape) => Answer::bad_request(shape),
            },
            Err(refused) => refused,
        },
        (_, None | Some("seal" | "unseal" | "passphrase")) => Answer::new(
            405,
            r#"{"error":"that route takes another method"}"#.to_owned(),
        ),
        _ => Answer::new(404, r#"{"error":"no such route"}"#.to_owned()),
    }
}
