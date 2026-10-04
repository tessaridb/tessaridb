use tessaridb::{Error, Outcome, Value};

use super::scripts::encode;
use super::{failure, json};

fn rendered(outcome: &Outcome) -> String {
    let mut body = String::new();
    encode(&mut body, outcome, &json::Names::new());
    body
}

/// The redirect this node would answer with, as the session raises it.
fn sent_elsewhere() -> Error {
    Error::ReadIsElsewhere {
        because: "a staleness bound of 60s".to_owned(),
        endpoint: "two.example:9080".to_owned(),
        node: [3; tessari_encoding::NODE_ID_LEN],
        epoch: tessari_types::Epoch::new(7),
        span: tessari_ql::Span::new(0, 3),
    }
}

#[test]
fn a_redirect_leaves_this_surface_as_a_307_and_not_as_a_bad_request() {
    // It used to reach the catch-all and answer `400`, which tells a caller
    // they wrote the request wrongly — the one thing they did not do. This
    // is the same correction `NotGranted`, `RecordExists` and
    // `StillDepended` each needed, and for the same reason.
    let answer = failure(&sent_elsewhere());
    assert_eq!(answer.status, 307);
}

#[test]
fn a_redirect_carries_the_address_in_the_header_and_not_only_in_the_prose() {
    // RFC 9110: a `307` without a `Location` is not a redirect a client can
    // act on. A client that had to parse the endpoint out of an error
    // message would be doing by hand what the status exists to make
    // automatic.
    let answer = failure(&sent_elsewhere());
    assert_eq!(answer.location.as_deref(), Some("two.example:9080"));
}

#[test]
fn an_aborted_transaction_across_leaders_answers_with_the_kind_of_its_refusal() {
    // It reached the catch-all and answered `400` — *you wrote it wrong,
    // do not retry* — for an abort a conflict, a lapse or an unreachable
    // leader caused, which is exactly the work a client then loses
    // (Q-924). The kind is the answering node's, judged by this mapping.
    use tessaridb::{AcrossRefusal, PartRefused, RefusalKind};
    let span = tessari_ql::Span::new(0, 6);
    let there = |kind| Error::AcrossAborted {
        refusal: AcrossRefusal::There(PartRefused {
            kind,
            reason: "b refused".to_owned(),
        }),
        span,
    };
    assert_eq!(failure(&there(RefusalKind::Retriable)).status, 409);
    assert_eq!(failure(&there(RefusalKind::Forbidden)).status, 403);
    assert_eq!(failure(&there(RefusalKind::Invalid)).status, 400);
    // This node's own refusal answers as itself would have.
    let here = Error::AcrossAborted {
        refusal: AcrossRefusal::Here(Box::new(Error::NoBackupFolder)),
        span,
    };
    assert_eq!(failure(&here).status, 409);
    // In doubt is a store that could not confirm what it did, like a
    // commit a majority did not confirm in time: read, then decide.
    let doubt = Error::AcrossInDoubt {
        reason: "the record's leader did not answer".to_owned(),
        span,
    };
    assert_eq!(failure(&doubt).status, 409);
}

#[test]
fn a_refusal_carried_to_another_node_keeps_the_kind_its_status_says() {
    use tessaridb::RefusalKind;
    assert_eq!(
        super::refusal_kind(&Error::NoBackupFolder),
        RefusalKind::Retriable
    );
    assert_eq!(
        super::refusal_kind(&Error::SignInThrottled),
        RefusalKind::Retriable
    );
    assert_eq!(
        super::refusal_kind(&Error::SignInRefused),
        RefusalKind::Forbidden
    );
    assert_eq!(
        super::refusal_kind(&Error::MayNotTravel {
            statement: "DEFINE USER"
        }),
        RefusalKind::Forbidden
    );
    assert_eq!(
        super::refusal_kind(&Error::PasswordEmpty {
            span: tessari_ql::Span::new(0, 1)
        }),
        RefusalKind::Invalid
    );
}

#[test]
fn an_ordinary_refusal_carries_no_location() {
    // The field is about redirects and nothing else; a `Location` on a
    // refusal would send a client somewhere over a failure that had no
    // *somewhere*.
    let answer = failure(&Error::SignInRefused);
    assert_eq!(answer.status, 401);
    assert!(answer.location.is_none());
}

#[test]
fn a_conditional_deletes_count_reaches_the_caller() {
    // `done` would make an operator run a count before and after to learn
    // what their retention policy did. `unknown` — which is what this
    // answered — tells them their client is out of date instead.
    assert_eq!(
        rendered(&Outcome::Removed { count: 12_043 }),
        r#"{"kind":"removed","count":12043}"#
    );
}

#[test]
fn every_outcome_this_build_knows_has_its_own_kind() {
    // The guard, and the reason this test exists rather than a review note:
    // the wildcard arm below `Removed` is *correct* and cannot be removed,
    // because `Outcome` is `#[non_exhaustive]`. Nothing tells a correct
    // wildcard apart from one swallowing a known outcome except listing the
    // variants and checking each renders as itself.
    //
    // `Outcome::Records` is not here because building a `Plan` by hand adds
    // a dozen lines of fixture; it is covered against a real node by
    // `tests/routes.rs`, which asserts `"kind":"records"`. A new variant
    // added to `Outcome` belongs in this list.
    for (outcome, expected) in [
        (Outcome::Done, "done"),
        (Outcome::Value(Value::Null), "value"),
        (Outcome::Keys(Vec::new()), "keys"),
        (Outcome::Removed { count: 0 }, "removed"),
    ] {
        let body = rendered(&outcome);
        assert!(
            body.contains(&format!(r#""kind":"{expected}""#)),
            "{outcome:?} rendered as {body}"
        );
        assert!(
            !body.contains(r#""kind":"unknown""#),
            "{outcome:?} rendered as unknown: {body}"
        );
    }
}

#[test]
fn a_refusal_that_is_not_the_callers_fault_does_not_answer_400() {
    use tessari_ql::Span;
    use tessari_session::{Depended, Error};

    // The catch-all below the named arms answers `400` — "you wrote it
    // wrong" — and for most of what the session raises that is true. These
    // nine were reaching it while belonging to a row the protocol
    // specification already publishes (§5.2): a client branches on the
    // status, and `400` tells it to stop retrying and fix its request, which
    // is the one thing that never helps for any of these.
    let at = Span::new(0, 1);
    let user = || "someone".to_owned();
    let cases: Vec<(u16, Error)> = vec![
        // Authenticated, and the answer is still no. Signing in again never
        // helps, which is the whole reason 401 and 403 are kept apart.
        (
            403,
            Error::GrantedUserCannotBackUp {
                user: user(),
                span: at,
            },
        ),
        (
            403,
            Error::GrantedUserCannotDeclare {
                user: user(),
                span: at,
            },
        ),
        (
            403,
            Error::NotYours {
                user: user(),
                span: at,
            },
        ),
        (
            403,
            Error::WiderThanYou {
                user: user(),
                span: at,
            },
        ),
        // Written right, and the data says no. Retriable after a change —
        // the file's own definition of the 409 it already gives `Store`.
        (
            409,
            Error::AcrossSettling {
                table: "follows".to_owned(),
                span: at,
            },
        ),
        (
            409,
            Error::RecordExists {
                id: "users:1".to_owned(),
                span: at,
            },
        ),
        (
            409,
            Error::StillDepended {
                depended: Depended::DatabaseByTable,
                name: "d".to_owned(),
                count: 1,
                first: "t".to_owned(),
                span: at,
            },
        ),
        // A device or an invariant, not a caller. `BackupFailed` at 400 puts
        // a failed write behind "you asked wrongly", where no alert reads it.
        (
            500,
            Error::BackupFailed {
                reason: "the device is full".to_owned(),
            },
        ),
        (
            500,
            Error::IdentityUnavailable {
                reason: "exhausted",
                span: at,
            },
        ),
        (500, Error::FoldOutsideAGroup { span: at }),
    ];
    for (expected, error) in cases {
        assert_eq!(
            super::failure(&error).status,
            expected,
            "{error} answered the wrong status"
        );
    }
}
