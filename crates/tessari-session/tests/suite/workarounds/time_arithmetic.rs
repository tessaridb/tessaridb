//! A datetime moved by a duration, and the duration between two (ADR-0124 D4).

use tessari_session::Error;
use tessari_types::Value;

use super::{inside, refused, store, value};

fn yes(session: &mut tessari_session::Session<'_>, test: &str) {
    assert_eq!(
        value(session, &format!("RETURN {test};")),
        Value::Bool(true),
        "{test}"
    );
}

#[test]
fn a_datetime_moves_by_a_duration_either_way() {
    let store = store();
    let mut session = inside(&store);
    yes(
        &mut session,
        "datetime '2026-10-08T00:00:00Z' + 90s = datetime '2026-10-08T00:01:30Z'",
    );
    yes(
        &mut session,
        "90s + datetime '2026-10-08T00:00:00Z' = datetime '2026-10-08T00:01:30Z'",
    );
    yes(
        &mut session,
        "datetime '2026-10-08T00:00:00Z' - 1d = datetime '2026-10-07T00:00:00Z'",
    );
    yes(&mut session, "time::now() - 10m < time::now()");
}

#[test]
fn two_datetimes_differ_by_a_duration_that_may_be_negative() {
    let store = store();
    let mut session = inside(&store);
    yes(
        &mut session,
        "datetime '2026-10-08T01:00:00Z' - datetime '2026-10-08T00:00:00Z' = 1h",
    );
    yes(
        &mut session,
        "datetime '2026-10-08T00:00:00Z' - datetime '2026-10-08T01:00:00Z' = -1h",
    );
    yes(&mut session, "1h + 30m = 90m");
    yes(&mut session, "1h - 30m = 30m");
}

#[test]
fn a_duration_is_built_from_seconds() {
    let store = store();
    let mut session = inside(&store);
    yes(&mut session, "duration::from_secs(600) = 10m");
    yes(&mut session, "duration::from_secs(1.5) = 1500ms");
    yes(
        &mut session,
        "datetime '2026-10-08T00:00:00Z' + duration::from_secs(60) = datetime '2026-10-08T00:01:00Z'",
    );
    let error = refused(&mut session, "RETURN duration::from_secs('ten');");
    assert!(matches!(error, Error::WrongArgument { .. }), "{error:?}");
}

#[test]
fn a_result_outside_the_range_is_refused_rather_than_wrapped() {
    let store = store();
    let mut session = inside(&store);
    for overflowing in [
        "RETURN (datetime '2026-10-08T00:00:00Z' + duration::from_secs(5000000000000000000)) \
         + duration::from_secs(5000000000000000000);",
        "RETURN duration::from_secs(5000000000000000000) + duration::from_secs(5000000000000000000);",
        "RETURN duration::from_secs(10000000000000000000000.0);",
    ] {
        let error = refused(&mut session, overflowing);
        assert!(
            matches!(
                error,
                Error::ArithmeticFailed { .. } | Error::CallFailed { .. }
            ),
            "{overflowing}: {error:?}"
        );
    }
}
