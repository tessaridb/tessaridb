//! `RELEASE q:id NOT BEFORE …` — hand work back for later in one write
//! (ADR-0124 D6).

use tessari_session::{Error, Outcome};
use tessari_types::Value;

use super::{inside, refused, rows, run, store, value};

fn claimed(session: &mut tessari_session::Session<'_>, statement: &str) -> usize {
    match run(session, statement) {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("{statement}: {other:?}"),
    }
}

fn mail_with_one_held(session: &mut tessari_session::Session<'_>) {
    run(
        session,
        "DEFINE QUEUE mail TIMEOUT 5m NOT BEFORE send_at; CREATE mail:1 = { to: 'ada' };",
    );
    assert_eq!(claimed(session, "CLAIM FROM mail;"), 1);
}

#[test]
fn a_record_released_for_later_is_not_handed_out_before_then() {
    let store = store();
    let mut session = inside(&store);
    mail_with_one_held(&mut session);
    run(&mut session, "RELEASE mail:1 NOT BEFORE 1h;");
    assert_eq!(claimed(&mut session, "CLAIM FROM mail;"), 0, "held back");
    assert_eq!(claimed(&mut session, "CLAIM mail:1;"), 0, "by name too");
    assert_eq!(
        value(
            &mut session,
            "RETURN (SELECT send_at FROM ONLY mail:1).send_at > time::now() + 59m;"
        ),
        Value::Bool(true),
        "the declared field holds the instant"
    );
}

#[test]
fn the_hold_is_cleared_in_the_same_write() {
    let store = store();
    let mut session = inside(&store);
    mail_with_one_held(&mut session);
    run(
        &mut session,
        "RELEASE mail:1 NOT BEFORE datetime '2000-01-01T00:00:00Z';",
    );
    assert_eq!(
        claimed(&mut session, "CLAIM FROM mail;"),
        1,
        "an instant already passed delays nothing, and the hold is gone"
    );
}

#[test]
fn release_all_for_later_moves_every_hold() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE mail TIMEOUT 5m NOT BEFORE send_at; CREATE mail:1 = {}; CREATE mail:2 = {};",
    );
    run(&mut session, "USE CONSUMER 'mailer';");
    assert_eq!(claimed(&mut session, "CLAIM 2 FROM mail;"), 2);
    run(&mut session, "RELEASE ALL FROM mail NOT BEFORE 1h;");
    assert_eq!(claimed(&mut session, "CLAIM 2 FROM mail;"), 0);
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM mail WHERE send_at > time::now();"
        )
        .len(),
        2
    );
}

#[test]
fn a_queue_with_no_delay_field_refuses_it_by_name() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE jobs TIMEOUT 5m; CREATE jobs:1 = {};",
    );
    run(&mut session, "CLAIM FROM jobs;");
    let error = refused(&mut session, "RELEASE jobs:1 NOT BEFORE 1h;");
    assert!(matches!(error, Error::NoDelayField { .. }), "{error:?}");
}

#[test]
fn an_instant_that_is_neither_a_datetime_nor_a_duration_is_refused() {
    let store = store();
    let mut session = inside(&store);
    mail_with_one_held(&mut session);
    let error = refused(&mut session, "RELEASE mail:1 NOT BEFORE 'tomorrow';");
    assert!(matches!(error, Error::NotAnInstant { .. }), "{error:?}");
    assert_eq!(
        claimed(&mut session, "CLAIM FROM mail;"),
        0,
        "still held: nothing was written"
    );
}
