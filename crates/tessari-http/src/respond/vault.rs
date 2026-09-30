//! `GET /vault`, `POST /vault/unseal`, `POST /vault/seal` and
//! `POST /vault/passphrase` (ADR-0092 D2, D3).
//!
//! A surface over [`tessaridb::Session::vault`] rather than a second
//! implementation: who may unseal, the throttle and the answer are the
//! statement's own. What the route adds is only the transport — the passphrase
//! arrives as a body of its own, which is not a script and carries no grammar,
//! the shape `POST /password` already has. The body is never logged, echoed or
//! quoted in a refusal.

use tessaridb::{Db, VaultAct};

use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::json;
use crate::tokens::Tokens;

/// Carry out `act` as the caller and answer with the seal status as JSON.
pub(crate) fn answer(db: &Db, act: VaultAct<'_>, tokens: &Tokens, presented: &Presented) -> Answer {
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(refused) => return refused,
    };
    match session.vault(act) {
        Ok(status) => {
            let mut body = String::new();
            json::write(&mut body, &status, &json::Names::new());
            Answer::new(200, body)
        }
        Err(error) => failure(&error),
    }
}
