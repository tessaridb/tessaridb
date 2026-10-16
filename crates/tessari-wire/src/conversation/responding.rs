use super::*;
use tessari_types::RefusalClass;

/// Answer one request: sign in if it carries credentials, then run it.
///
/// Runs on the blocking pool — a password hash, a statement and a forwarded
/// write all block.
pub(crate) async fn respond(
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    request: &Request,
    theirs: u8,
) -> Answer {
    let refusal = |class, message: String| Answer {
        kind: frame::Kind::Refusal,
        body: frame::refusal(theirs, class, &message),
        redirect: None,
        snapshot: None,
    };
    if let Some((name, password)) = &request.credentials
        && let Err(refused) = session.sign_in(name, password).await
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
    let spool = if request.stream && theirs >= frame::STREAMS {
        match spooled(session) {
            Ok(spool) => Some(spool),
            Err(why) => {
                return refusal(
                    RefusalClass::Unavailable,
                    format!(
                        "the node could not open a file to stream the snapshot through: {}",
                        why.kind()
                    ),
                );
            }
        }
    } else {
        None
    };
    let ran = session.run_with(&request.script, &request.parameters).await;
    // Cleared whatever the script did, so a sink it never took cannot catch the
    // next request's snapshot.
    let streamed = spool.filter(|_| session.snapshot_was_written());
    // Coordinated (ADR-0108 D1): a write this node may not take, or a request
    // whose caller cannot follow a redirect, is carried over the peer link to
    // the node that can answer it, as the caller this session verified — never
    // with the caller's password. Matched on the **variant**, never on text.
    if let Err(refused) = &ran
        && !session.landed()
        && (matches!(refused, tessaridb::Error::NotWritable { .. }) || theirs < frame::REDIRECTS)
        && tessaridb::travels(&request.script)
        && let Some(coordinator) = db.coordinator()
        && let Some(to) = db.answers_instead(refused).await
    {
        let carried = coordinator
            .coordinate(&tessaridb::Coordination {
                to,
                user: session.signed_in(),
                namespace: selected.0.as_deref(),
                database: selected.1.as_deref(),
                script: &request.script,
                parameters: &request.parameters,
                surface: tessaridb::Surface::Wire { minor: theirs },
            })
            .await;
        return match carried {
            Ok(answer) => match u8::try_from(answer.kind)
                .ok()
                .and_then(frame::Kind::from_tag)
            {
                Some(kind) => Answer {
                    kind,
                    body: answer.body,
                    redirect: None,
                    snapshot: None,
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
            match db.writable_peer().await {
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
        && let Some(sent) = redirected(db, &ran).await
    {
        return Answer {
            kind: frame::Kind::Elsewhere,
            body: sent.encode(),
            redirect: Some(sent.settlement == redirect::Settlement::Settled),
            snapshot: None,
        };
    }
    let mut answer = render(db, &ran, theirs).await;
    // Parts precede only an answer: a refusal ends the request without them,
    // and the spooled file goes with it.
    if answer.kind == frame::Kind::Answer {
        answer.snapshot = streamed;
    }
    answer
}

/// Spools this process has made, so two at once never share a name. A count
/// and nothing else, so `Relaxed` is all it needs.
static SPOOLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An unlinked file in the temporary folder, set as the session's sink for the
/// snapshot the next script takes (ADR-0125 D5).
///
/// Unlinked as soon as it is open, as `GET /backup`'s spool is: the open handle
/// keeps the bytes, and nothing outlives the request whatever ends it.
fn spooled(session: &mut tessaridb::Session<'_>) -> std::io::Result<std::fs::File> {
    let path = std::env::temp_dir().join(format!(
        "tessaridb-stream-{}-{}.tessarisnap",
        std::process::id(),
        SPOOLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    session.snapshot_next_into(Box::new(std::io::BufWriter::new(file.try_clone()?)));
    Ok(file)
}

/// A run's answer as this surface writes it: one outcome per statement, or the
/// refusal in the store's own words.
pub(super) async fn render(
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
                let names = message::names_for(db, outcome).await;
                answer.extend_from_slice(&message::encode_outcome(outcome, &names));
            }
            // Checked here, where every answer body is made, and not at the
            // write: a failed write ends the connection, and the client reads
            // that as a network fault rather than as an answer about its request.
            if u32::try_from(answer.len()).map_or(true, |length| length > frame::CEILING) {
                let too_large = Error::AnswerTooLarge {
                    length: answer.len(),
                    ceiling: frame::CEILING,
                };
                return Answer {
                    kind: frame::Kind::Refusal,
                    body: frame::refusal(theirs, RefusalClass::Invalid, &too_large.to_string()),
                    redirect: None,
                    snapshot: None,
                };
            }
            Answer {
                kind: frame::Kind::Answer,
                body: answer,
                redirect: None,
                snapshot: None,
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
        snapshot: None,
    }
}

/// A carried request's answer, rendered for a wire client by the node that ran
/// it (ADR-0108 D1). A refusal naming yet another node is a refusal here: the
/// request has made its one hop.
pub async fn render_coordinated(
    db: &Db,
    ran: &tessaridb::Result<Vec<tessaridb::Outcome>>,
    minor: u8,
) -> tessaridb::Coordinated {
    let answer = render(db, ran, minor).await;
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
pub(crate) async fn respond_vault(
    session: &mut tessaridb::Session<'_>,
    asked: &crate::VaultAsk,
    theirs: u8,
) -> Answer {
    if let Some((name, password)) = &asked.credentials
        && let Err(refused) = session.sign_in(name, password).await
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
    match session.vault(target, act).await {
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
                snapshot: None,
            }
        }
        Err(refused) => refused_as(&refused, theirs),
    }
}
