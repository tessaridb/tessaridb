use super::*;
use tessari_types::RefusalClass;

/// A failure, as the status that says what kind it was.
pub(crate) fn failure(error: &Error) -> Answer {
    // Decided once, in the session (ADR-0117): the status is the class's, so the
    // wire's byte and this status cannot disagree about what to do next.
    let class = error.class();
    let status = status_of(class);
    let mut body = String::from(r#"{"error":"#);
    json::string(&mut body, &error.to_string());
    body.push_str(r#","code":"#);
    json::string(&mut body, class.word());
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

/// The status a class answers with (ADR-0117 D1).
pub(crate) const fn status_of(class: RefusalClass) -> u16 {
    match class {
        RefusalClass::Invalid => 400,
        RefusalClass::Unauthenticated => 401,
        RefusalClass::Forbidden => 403,
        RefusalClass::Throttled => 429,
        RefusalClass::Elsewhere => 307,
        RefusalClass::Retry | RefusalClass::Conflict => 409,
        RefusalClass::Unavailable => 503,
        RefusalClass::Internal => 500,
    }
}

/// The class an error answer built from a status alone carries — a route that
/// does not exist, a method a route does not take, a body past the ceiling.
/// Every refusal the session raises is classed by [`Error::class`] instead.
pub(crate) const fn class_of_status(status: u16) -> RefusalClass {
    match status {
        401 => RefusalClass::Unauthenticated,
        403 => RefusalClass::Forbidden,
        307 => RefusalClass::Elsewhere,
        409 => RefusalClass::Conflict,
        429 => RefusalClass::Throttled,
        503 => RefusalClass::Unavailable,
        500..=599 => RefusalClass::Internal,
        _ => RefusalClass::Invalid,
    }
}

/// An error body carrying its class, as every one must (ADR-0117 D4).
///
/// A refusal from the session already says its own; an answer built from a
/// status alone — no such route, not that method, a body past the ceiling — is
/// given the class its status means. Applied where every answer becomes a
/// response, so a route added later cannot forget it.
pub(crate) fn coded(status: u16, body: Vec<u8>) -> Vec<u8> {
    const OPENING: &[u8] = br#"{"error":"#;
    const CODE: &[u8] = br#""code":"#;
    if status < 300
        || !body.starts_with(OPENING)
        || body.windows(CODE.len()).any(|window| window == CODE)
    {
        return body;
    }
    let Some(close) = body.iter().rposition(|byte| *byte == b'}') else {
        return body;
    };
    let mut coded = Vec::with_capacity(body.len().saturating_add(24));
    coded.extend_from_slice(body.get(..close).unwrap_or_default());
    coded.extend_from_slice(br#","code":""#);
    coded.extend_from_slice(class_of_status(status).word().as_bytes());
    coded.extend_from_slice(br#""}"#);
    coded.extend_from_slice(body.get(close.saturating_add(1)..).unwrap_or_default());
    coded
}
