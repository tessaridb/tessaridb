use super::*;
use tessari_types::RefusalClass;

/// Answer one request: sign in if it carries credentials, then run it.
///
/// Runs on the blocking pool — a password hash, a statement and a forwarded
/// write all block.
pub(crate) fn respond(
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    request: &Request,
    theirs: u8,
) -> Answer {
    let refusal = |class, message: String| Answer {
        kind: frame::Kind::Refusal,
        body: frame::refusal(theirs, class, &message),
        redirect: None,
    };
    if let Some((name, password)) = &request.credentials
        && let Err(refused) = session.sign_in(name, password)
    {
        // The session's own refusal, travelling as one. A second rule here
        // would be a second place for "who may do this" to be decided.
        tracing::warn!(refusal = %refused, "request refused");
        return refused_as(&refused, theirs);
    }
    // What the caller had selected BEFORE the script ran: a carried request is
    // run again from the start on the node that answers it.
    let selected = (
        session.namespace().map(str::to_owned),
        session.database().map(str::to_owned),
    );
    let ran = session.run_with(&request.script, &request.parameters);
    // Coordinated (ADR-0108 D1): a write this node may not take, or a request
    // whose caller cannot follow a redirect, is carried over the peer link to
    // the node that can answer it, as the caller this session verified — never
    // with the caller's password. Matched on the **variant**, never on text.
    if let Err(refused) = &ran
        && !session.landed()
        && (matches!(refused, tessaridb::Error::NotWritable { .. }) || theirs < frame::REDIRECTS)
        && tessaridb::travels(&request.script)
        && let Some(coordinator) = db.coordinator()
        && let Some(to) = db.answers_instead(refused)
    {
        let carried = coordinator.coordinate(&tessaridb::Coordination {
            to,
            user: session.signed_in(),
            namespace: selected.0.as_deref(),
            database: selected.1.as_deref(),
            script: &request.script,
            parameters: &request.parameters,
            surface: tessaridb::Surface::Wire { minor: theirs },
        });
        return match carried {
            Ok(answer) => match u8::try_from(answer.kind)
                .ok()
                .and_then(frame::Kind::from_tag)
            {
                Some(kind) => Answer {
                    kind,
                    body: answer.body,
                    redirect: None,
                },
                None => refusal(
                    RefusalClass::Internal,
                    format!(
                        "the node that answered sent a kind this node does not know ({})",
                        answer.kind
                    ),
                ),
            },
            // The hop failed, and the client is told that rather than being
            // told the statement was wrong. It was not.
            Err(why) => refusal(RefusalClass::Unavailable, why),
        };
    }
    // No peer link, so nothing to carry it over: the write is refused, naming
    // where the writable peer takes writes. The caller's password stays here
    // (ADR-0108 D1, R-10) — it used to be relayed to that address in clear.
    if matches!(ran, Err(tessaridb::Error::NotWritable { .. })) {
        return refusal(
            RefusalClass::Unavailable,
            match db.writable_peer() {
                Ok(Some(peer)) => format!(
                    "this node does not take writes; the peer declared writable takes them at {}",
                    peer.clients.unwrap_or(peer.endpoint)
                ),
                Ok(None) => {
                    "this node does not accept writes, and no peer is declared writable".to_owned()
                }
                Err(why) => why.to_string(),
            },
        );
    }
    // A redirect is an **instruction** and leaves as its own frame rather than
    // as a refusal carrying a hint (`redirect.rs`), gated on what the client
    // said at the greeting: a client built before tag 13 cannot name the frame,
    // and the refusal it has always received is the better answer for it.
    //
    // And gated on nothing having landed (ADR-0101 D3): the client follows by
    // sending this whole script again, so a script that already committed part
    // of itself gets the refusal rather than an invitation to commit it twice.
    if theirs >= frame::REDIRECTS
        && !session.landed()
        && let Some(sent) = redirected(db, &ran)
    {
        return Answer {
            kind: frame::Kind::Elsewhere,
            body: sent.encode(),
            redirect: Some(sent.settlement == redirect::Settlement::Settled),
        };
    }
    render(db, &ran, theirs)
}

/// A run's answer as this surface writes it: one outcome per statement, or the
/// refusal in the store's own words.
pub(super) fn render(
    db: &Db,
    ran: &tessaridb::Result<Vec<tessaridb::Outcome>>,
    theirs: u8,
) -> Answer {
    match ran {
        Ok(outcomes) => {
            let mut answer = Vec::new();
            frame::put_u32(
                &mut answer,
                u32::try_from(outcomes.len()).unwrap_or(u32::MAX),
            );
            for outcome in outcomes {
                // Resolved here because the catalog is here. `names_in` walks
                // the answer first and touches nothing when it holds no
                // reference, which is most answers.
                let names = message::names_for(db, outcome);
                answer.extend_from_slice(&message::encode_outcome(outcome, &names));
            }
            Answer {
                kind: frame::Kind::Answer,
                body: answer,
                redirect: None,
            }
        }
        // A refusal does not close the connection: a client that mistyped a
        // statement has not stopped being a client.
        Err(refused) => refused_as(refused, theirs),
    }
}

/// A refusal for a client of minor `theirs`, carrying its class when the client
/// can read one (ADR-0117).
pub(crate) fn refused_as(refused: &tessaridb::Error, theirs: u8) -> Answer {
    Answer {
        kind: frame::Kind::Refusal,
        body: frame::refusal(theirs, refused.class(), &refused.to_string()),
        redirect: None,
    }
}

/// A carried request's answer, rendered for a wire client by the node that ran
/// it (ADR-0108 D1). A refusal naming yet another node is a refusal here: the
/// request has made its one hop.
#[must_use]
pub fn render_coordinated(
    db: &Db,
    ran: &tessaridb::Result<Vec<tessaridb::Outcome>>,
    minor: u8,
) -> tessaridb::Coordinated {
    let answer = render(db, ran, minor);
    tessaridb::Coordinated {
        kind: u16::from(answer.kind.tag()),
        body: answer.body,
    }
}

/// Carry out one vault frame as the caller (ADR-0092 D2).
///
/// Signs in as a request does, then asks the session's own vault surface, so
/// who may unseal, the throttle and the answer are the statement's. A refusal is
/// the session's own words, which never quote the passphrase.
pub(crate) fn respond_vault(
    session: &mut tessaridb::Session<'_>,
    asked: &crate::VaultAsk,
    theirs: u8,
) -> Answer {
    if let Some((name, password)) = &asked.credentials
        && let Err(refused) = session.sign_in(name, password)
    {
        tracing::warn!(refusal = %refused, "request refused");
        return refused_as(&refused, theirs);
    }
    let act = match &asked.call {
        crate::VaultCall::Status => tessaridb::VaultAct::Status,
        crate::VaultCall::Unseal(passphrase) => tessaridb::VaultAct::Unseal { passphrase },
        crate::VaultCall::Seal => tessaridb::VaultAct::Seal,
        crate::VaultCall::Change { current, new } => tessaridb::VaultAct::Change { current, new },
    };
    let target = asked
        .place
        .as_ref()
        .map_or(tessaridb::VaultTarget::Store, |place| {
            tessaridb::VaultTarget::Vault {
                namespace: &place.namespace,
                database: &place.database,
                vault: &place.vault,
            }
        });
    match session.vault(target, act) {
        Ok(status) => {
            let mut body = Vec::new();
            frame::put_u32(&mut body, 1);
            body.extend_from_slice(&message::encode_outcome(
                &tessaridb::Outcome::Value(status),
                &message::Names::new(),
            ));
            Answer {
                kind: frame::Kind::Answer,
                body,
                redirect: None,
            }
        }
        Err(refused) => refused_as(&refused, theirs),
    }
}
