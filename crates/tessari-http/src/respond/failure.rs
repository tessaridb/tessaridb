use super::*;

/// A failure, as the status that says what kind it was.
pub(crate) fn failure(error: &Error) -> Answer {
    let status = match error {
        // A transaction across leaders that did not commit answers as the
        // refusal that stopped it: this node's own as itself, another node's by
        // the kind that node judged it with — which `refusal_kind` reads off
        // this same mapping there (Q-924). It used to reach the catch-all,
        // telling a caller whose abort was a conflict not to retry.
        Error::AcrossAborted { refusal, .. } => match refusal {
            tessaridb::AcrossRefusal::Here(cause) => failure(cause).status,
            tessaridb::AcrossRefusal::There(refused) => match refused.kind {
                tessaridb::RefusalKind::Retriable => 409,
                tessaridb::RefusalKind::Forbidden => 403,
                tessaridb::RefusalKind::Invalid => 400,
            },
        },
        // This node does not know who is asking: no credential against a closed
        // store, or one it refused. Both are answered the same way, because
        // telling them apart tells an attacker which half to keep guessing at.
        // A token whose account has since changed belongs here too, and for the
        // same reason it is not a 403: the holder was somebody, the store no
        // longer agrees, and what fixes it is signing in again.
        Error::NotSignedIn { .. }
        | Error::SignInRefused
        | Error::TicketStale
        // The caller is signed in and the second proof failed, which is still
        // "identify yourself" — a client acts on it by asking for the password
        // again, exactly as for the first.
        | Error::CurrentPasswordRefused => 401,
        // It declined to look. Not a 401, because a client told "wrong" retries
        // with a different password and one told "too many" must retry with the
        // same one later — and 429 is the status every client library already
        // backs off on.
        // A public topic's anonymous allowance is spent: the same back-off, for
        // the same reason, and it is earned back over the topic's window.
        Error::SignInThrottled | Error::PassphraseThrottled | Error::TopicRateExceeded { .. } => {
            429
        }
        // It knows, and the answer is still no. A different thing entirely, and
        // a client that cannot tell retries a signin that will never help.
        //
        // `NotGranted` belongs here for exactly that reason and was reaching the
        // catch-all instead: a caller whose grants do not cover the table was
        // being told they had written the request wrongly, which is the one
        // thing they could not fix.
        // `NotTheWholeStore` belongs with these and not with `401`: the node
        // knows exactly who is asking, and signing in again will never help.
        // `CannotHandOut` is here for the same reason and not with the 400s: a
        // caller trying to grant past their own holdings wrote the statement
        // exactly right, and the refusal is about who they are.
        // These four were reaching the catch-all for the same reason
        // `NotGranted` did. A grant-governed user asking for a backup or trying
        // to declare structure wrote a statement this store understands
        // perfectly; an owner reaching a user outside their own tenancy, or
        // declaring somebody who would reach further than they do, likewise.
        // Every one of them is `CannotHandOut`'s case — the statement is right
        // and the refusal is about who is asking.
        Error::RoleForbids { .. }
        | Error::OutsideTenancy { .. }
        | Error::NotGranted { .. }
        | Error::NotTheWholeStore { .. }
        | Error::CannotHandOut { .. }
        | Error::GrantedUserCannotBackUp { .. }
        | Error::GrantedUserCannotDeclare { .. }
        | Error::NotYours { .. }
        | Error::WiderThanYou { .. }
        // Carried here from another node, and authority or membership changes
        // only for a caller signed in to the node that judges it (ADR-0108 D2).
        | Error::MayNotTravel { .. } => 403,
        // The caller wrote it wrong, and no amount of changing the data helps.
        // A new password that is not one is a bad request rather than a
        // refusal: nothing about the caller's authority is in question.
        Error::PasswordEmpty { .. } => 400,
        Error::Script(_) => 400,
        // Not a failure at all. It is here because this surface has one door for
        // everything the session returns, and it leaves through a different one.
        //
        // `307` and not `302`: only the temporary-redirect status promises that
        // the method and the body survive the hop, and a `POST /script` whose
        // script a client quietly dropped on the way to the other node is a
        // worse outcome than the refusal this used to be. Not `301` or `308`
        // either — both say *permanently*, and a redirect taken on how stale a
        // copy is right now is the least permanent fact this store holds.
        Error::ReadIsElsewhere { .. }
        | Error::Store(tessaridb::StoreError::WriteIsElsewhere { .. }) => 307,
        // The caller wrote it right and the data says no. Retriable after a
        // change, which is the whole reason this is not a 400.
        //
        // The session raises its own two of these rather than wrapping a store
        // error, so they were answering 400 while meaning exactly what this arm
        // means: a `CREATE` over a record that exists succeeds once the record
        // goes, and a drop blocked by a dependency succeeds once the dependant
        // does. A client told `400` stops retrying, which is the one response
        // that never becomes right.
        Error::Store(_)
        | Error::RecordExists { .. }
        | Error::StillDepended { .. }
        | Error::BackupExists { .. }
        | Error::RestoreTargetExists { .. }
        | Error::NoVaultRoot
        | Error::NoBackupFolder
        | Error::ShardMapMoved { .. }
        | Error::AcrossSettling { .. }
        // The decision was sent and not confirmed: like a commit a majority
        // did not confirm in time, the store is the one that does not know, and
        // a read of the records says what to do next.
        | Error::AcrossInDoubt { .. } => 409,
        // A substrate or decoding failure. Anything reaching here is a bug.
        //
        // A backup the writer could not write is a device speaking, not a
        // caller; an identity the store could not produce and a fold that
        // reached the evaluator are invariants of this build. Reported as 400
        // they read as user error and no alert ever sees them.
        Error::Encoding(_)
        | Error::BackupFailed { .. }
        | Error::IdentityUnavailable { .. }
        | Error::FoldOutsideAGroup { .. } => 500,
        // Everything else the session raises is about the script: an unselected
        // namespace, a wrong argument, a condition that is not a boolean.
        _ => 400,
    };
    let mut body = String::from(r#"{"error":"#);
    json::string(&mut body, &error.to_string());
    body.push('}');
    let mut answer = Answer::new(status, body);
    // The address travels in the header rather than only in the prose, for the
    // same reason the challenge travels beside a `401`: a redirect whose target
    // a client has to parse out of an error message is not a redirect.
    if let Some((endpoint, _)) = elsewhere(error) {
        answer.location = Some(endpoint.to_owned());
        // A write into a range another node leads names a leadership the caller
        // may remember; a read beyond its bound names a node for this read.
        answer.settled = Some(matches!(
            error,
            Error::Store(tessaridb::StoreError::WriteIsElsewhere { .. })
        ));
    }
    answer
}

/// The address and the node a refusal sends the caller to, when it is one of
/// the two that mean *go there* — a read beyond its bound, or a write into a
/// range another node leads (ADR-0101).
pub(crate) fn elsewhere(error: &Error) -> Option<(&str, &[u8; tessaridb::NODE_ID_LEN])> {
    match error {
        Error::ReadIsElsewhere { endpoint, node, .. }
        | Error::Store(tessaridb::StoreError::WriteIsElsewhere { endpoint, node, .. }) => {
            Some((endpoint.as_str(), node))
        }
        _ => None,
    }
}

/// A failure of a script that may have run part of itself before it failed.
///
/// A redirect invites the caller to send the same request to another node,
/// which is safe only while nothing in it has committed (ADR-0101 D3). A script
/// that has is answered `409` with the refusal's own words instead. A redirect
/// that is safe names the node's HTTP base plus `path` when its member row
/// declares one (`HTTP AT`), and the address the refusal carried otherwise.
pub(crate) fn script_failure(db: &Db, error: &Error, landed: bool, path: &str) -> Answer {
    let Some((_, node)) = elsewhere(error) else {
        return failure(error);
    };
    if landed {
        let mut body = String::from(r#"{"error":"#);
        json::string(
            &mut body,
            &format!(
                "{error} — not redirected, because part of this script had already \
                 committed here; send the rest to that node"
            ),
        );
        body.push('}');
        return Answer::new(409, body);
    }
    let mut answer = failure(error);
    if let Some(base) = db.member(node).ok().flatten().and_then(|row| row.http) {
        answer.location = Some(format!("{}{path}", base.trim_end_matches('/')));
    }
    answer
}

/// The kind of no `error` is, as the status this surface answers it with —
/// how a node that refused a part of a transaction across leaders tells the
/// asking node what its caller may do (Q-924).
#[must_use]
pub fn refusal_kind(error: &Error) -> tessaridb::RefusalKind {
    match failure(error).status {
        401 | 403 => tessaridb::RefusalKind::Forbidden,
        // A redirect, a conflict, a back-off, or a fault of this node: none of
        // them is about the caller's writes.
        307 | 409 | 429 | 500..=599 => tessaridb::RefusalKind::Retriable,
        _ => tessaridb::RefusalKind::Invalid,
    }
}
