//! A group whose members are named and whose messages wait for them (ADR-0133, W8d).
//!
//! `ACK DEADLINE NONE` keeps a message held until it is acknowledged or handed
//! out again by a statement, `MEMBER 'c'` records which member holds each
//! message and when it was delivered, `HELD` gives a member back what it holds,
//! and `INFO FOR GROUP` says all of it.

use std::collections::BTreeMap;
use std::time::Duration;

use tessari_session::{Error, Outcome, Session};
use tessari_types::{Number, Value};

use super::super::key_value::{on_each_backend, refused, run, value};
use super::opened;

fn appended(session: &mut Session<'_>, count: u64) {
    for n in 1..=count {
        run(session, &format!("CREATE events:'m{n}' = {{ n: {n} }};"));
    }
}

fn whole(fields: &BTreeMap<String, Value>, field: &str) -> u64 {
    match fields.get(field) {
        Some(Value::Number(Number::Integer(held))) => u64::try_from(*held).unwrap(),
        other => panic!("{field} is {other:?} in {fields:?}"),
    }
}

/// Every answered message as its fields.
fn messages(session: &mut Session<'_>, script: &str) -> Vec<BTreeMap<String, Value>> {
    match run(session, script) {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, body)| match body {
                Value::Object(fields) => fields,
                other => panic!("a message answered {other:?}"),
            })
            .collect(),
        other => panic!("{script} answered {other:?}"),
    }
}

/// Each answered message's position and how many times it has been handed out.
fn given(session: &mut Session<'_>, script: &str) -> Vec<(u64, u64)> {
    messages(session, script)
        .iter()
        .map(|fields| (whole(fields, "position"), whole(fields, "deliveries")))
        .collect()
}

fn report(session: &mut Session<'_>, group: &str) -> BTreeMap<String, Value> {
    match value(
        session,
        &format!("INFO FOR GROUP '{group}' ON TOPIC events;"),
    ) {
        Value::Object(fields) => fields,
        other => panic!("INFO FOR GROUP answered {other:?}"),
    }
}

fn held(report: &BTreeMap<String, Value>) -> Vec<BTreeMap<String, Value>> {
    match report.get("held") {
        Some(Value::Array(held)) => held
            .iter()
            .map(|entry| match entry {
                Value::Object(fields) => fields.clone(),
                other => panic!("a held message is {other:?}"),
            })
            .collect(),
        other => panic!("held is {other:?}"),
    }
}

#[test]
fn a_group_with_no_deadline_keeps_a_message_until_it_is_settled() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'waits' ON TOPIC events ACK DEADLINE NONE; \
             DEFINE GROUP 'lapses' ON TOPIC events ACK DEADLINE 1ms;",
        );
        appended(&mut session, 2);
        for group in ["waits", "lapses"] {
            let read = format!("READ FROM events FOR CONSUMER '{group}';");
            assert_eq!(given(&mut session, &read), vec![(1, 1)], "{}", backend.name);
        }
        std::thread::sleep(Duration::from_millis(30));
        // The control arm: a deadline that passed hands the message out again.
        assert_eq!(
            given(&mut session, "READ FROM events FOR CONSUMER 'lapses';"),
            vec![(1, 2)]
        );
        assert_eq!(
            given(&mut session, "READ FROM events FOR CONSUMER 'waits';"),
            vec![],
            "{}",
            backend.name
        );
        assert_eq!(
            report(&mut session, "waits").get("deadline"),
            Some(&Value::None)
        );
        assert_eq!(
            held(&report(&mut session, "waits"))[0].get("until"),
            Some(&Value::None)
        );
        run(&mut session, "NACK events FOR CONSUMER 'waits' AT 1;");
        assert_eq!(
            given(&mut session, "READ FROM events FOR CONSUMER 'waits';"),
            vec![(1, 2)]
        );
    });
}

#[test]
fn a_member_read_records_who_holds_each_message_and_since_when() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'mail' ON TOPIC events ACK DEADLINE NONE IN FLIGHT 10;",
        );
        appended(&mut session, 3);
        let first = messages(
            &mut session,
            "READ FROM events FOR CONSUMER 'mail' MEMBER 'alice' LIMIT 2;",
        );
        assert!(
            first
                .iter()
                .all(|fields| matches!(fields.get("appended"), Some(Value::Datetime(_)))),
            "{}: a group read answers when each message was appended: {first:?}",
            backend.name
        );
        assert_eq!(
            given(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' MEMBER 'bob';"
            ),
            vec![(3, 1)]
        );
        let report = report(&mut session, "mail");
        let holders: Vec<(u64, Value)> = held(&report)
            .iter()
            .map(|entry| {
                (
                    whole(entry, "position"),
                    entry.get("member").cloned().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            holders,
            vec![
                (1, Value::from("alice")),
                (2, Value::from("alice")),
                (3, Value::from("bob"))
            ],
            "{}",
            backend.name
        );
        assert!(
            held(&report)
                .iter()
                .all(|entry| matches!(entry.get("delivered"), Some(Value::Datetime(_)))),
            "{report:?}"
        );
        let Some(Value::Object(members)) = report.get("members") else {
            panic!("members is {:?}", report.get("members"));
        };
        let Some(Value::Object(alice)) = members.get("alice") else {
            panic!("no alice in {members:?}");
        };
        assert_eq!(whole(alice, "held"), 2);
        assert!(
            matches!(alice.get("seen"), Some(Value::Datetime(_))),
            "{alice:?}"
        );
        assert!(
            matches!(alice.get("active"), Some(Value::Datetime(_))),
            "{alice:?}"
        );
        assert_eq!(whole(&report, "in_flight"), 3);
    });
}

#[test]
fn held_gives_a_member_back_its_own_messages_and_counts_the_delivery() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'mail' ON TOPIC events ACK DEADLINE NONE IN FLIGHT 10;",
        );
        appended(&mut session, 4);
        run(
            &mut session,
            "READ FROM events FOR CONSUMER 'mail' MEMBER 'alice' LIMIT 2;",
        );
        run(
            &mut session,
            "READ FROM events FOR CONSUMER 'mail' MEMBER 'bob' LIMIT 1;",
        );
        run(&mut session, "ACK events FOR CONSUMER 'mail' AT 1;");
        assert_eq!(
            given(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' MEMBER 'alice' HELD;"
            ),
            vec![(2, 2)],
            "{}",
            backend.name
        );
        assert_eq!(
            given(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' MEMBER 'alice' HELD AFTER 2;"
            ),
            vec![]
        );
        assert_eq!(
            given(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' MEMBER 'bob' HELD;"
            ),
            vec![(3, 2)]
        );
        // A read of what is held hands nothing new out.
        assert_eq!(whole(&report(&mut session, "mail"), "position"), 3);
    });
}

#[test]
fn a_member_needs_a_group_and_held_needs_a_member() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'mail' ON TOPIC events ACK DEADLINE 30s; CREATE events:'m1' = { n: 1 };",
        );
        for (script, expected) in [
            (
                "READ FROM events FOR CONSUMER 'solo' MEMBER 'alice';",
                "so it has no members",
            ),
            ("READ FROM events MEMBER 'alice';", "`FOR CONSUMER"),
            ("READ FROM events FOR CONSUMER 'mail' HELD;", "MEMBER"),
            ("INFO FOR GROUP 'none' ON TOPIC events;", "no group named"),
        ] {
            let why = refused(&mut session, script);
            assert!(why.contains(expected), "{}: {script}: {why}", backend.name);
        }
        assert!(matches!(
            session_error(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' AFTER 0;"
            ),
            Error::AfterOnGroup { .. }
        ));
    });
}

fn session_error(session: &mut Session<'_>, script: &str) -> Error {
    use tessari_session::RunNow;
    match session.run_now(script) {
        Err(error) => error,
        Ok(outcomes) => panic!("{script} answered {outcomes:?}"),
    }
}

fn holders(session: &mut Session<'_>) -> Vec<(u64, Value)> {
    held(&report(session, "mail"))
        .iter()
        .map(|entry| {
            (
                whole(entry, "position"),
                entry.get("member").cloned().unwrap(),
            )
        })
        .collect()
}

fn mail_held_by_alice(session: &mut Session<'_>, count: u64) {
    run(
        session,
        "DEFINE GROUP 'mail' ON TOPIC events ACK DEADLINE NONE IN FLIGHT 10;",
    );
    appended(session, count);
    run(
        session,
        &format!("READ FROM events FOR CONSUMER 'mail' MEMBER 'alice' LIMIT {count};"),
    );
}

#[test]
fn hand_gives_held_messages_to_another_member() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        mail_held_by_alice(&mut session, 2);
        assert_eq!(
            given(
                &mut session,
                "HAND events FOR CONSUMER 'mail' AT 1, 5 TO 'bob';"
            ),
            vec![(1, 2)],
            "{}: a position nothing holds is passed over",
            backend.name
        );
        assert_eq!(
            holders(&mut session),
            vec![(1, Value::from("bob")), (2, Value::from("alice"))]
        );
        assert_eq!(
            given(
                &mut session,
                "READ FROM events FOR CONSUMER 'mail' MEMBER 'bob' HELD;"
            ),
            vec![(1, 3)]
        );
    });
}

#[test]
fn hand_after_a_position_takes_what_has_been_idle_long_enough_up_to_the_limit() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        mail_held_by_alice(&mut session, 3);
        std::thread::sleep(Duration::from_millis(30));
        // The control arm: nothing has been idle for an hour.
        assert_eq!(
            given(
                &mut session,
                "HAND events FOR CONSUMER 'mail' AFTER 0 LIMIT 10 TO 'bob' IF IDLE 1h;"
            ),
            vec![]
        );
        assert_eq!(
            given(
                &mut session,
                "HAND events FOR CONSUMER 'mail' AFTER 1 LIMIT 1 TO 'bob' IF IDLE 10ms;"
            ),
            vec![(2, 2)],
            "{}",
            backend.name
        );
        assert_eq!(
            holders(&mut session),
            vec![
                (1, Value::from("alice")),
                (2, Value::from("bob")),
                (3, Value::from("alice"))
            ]
        );
    });
}

#[test]
fn hand_drops_a_held_message_retention_removed_and_says_so() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        mail_held_by_alice(&mut session, 2);
        run(&mut session, "TRIM events BEFORE 2;");
        match run(
            &mut session,
            "HAND events FOR CONSUMER 'mail' AT 1, 2 TO 'bob';",
        ) {
            Outcome::Records { records, notes, .. } => {
                assert_eq!(records.len(), 1, "{}", backend.name);
                assert!(
                    notes.iter().any(|note| matches!(
                        note,
                        tessari_session::Note::Lapsed { missed: 1, .. }
                    )),
                    "{}: {notes:?}",
                    backend.name
                );
            }
            other => panic!("HAND answered {other:?}"),
        }
        assert_eq!(holders(&mut session), vec![(2, Value::from("bob"))]);
    });
}

#[test]
fn a_member_is_added_and_dropped_with_what_it_held() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        mail_held_by_alice(&mut session, 2);
        let add = "ALTER GROUP 'mail' ON TOPIC events ADD MEMBER 'carol';";
        assert_eq!(value(&mut session, add), Value::Number(Number::Integer(1)));
        assert_eq!(value(&mut session, add), Value::Number(Number::Integer(0)));
        assert_eq!(
            value(
                &mut session,
                "ALTER GROUP 'mail' ON TOPIC events DROP MEMBER 'alice';"
            ),
            Value::Number(Number::Integer(2)),
            "{}",
            backend.name
        );
        assert_eq!(holders(&mut session), vec![]);
        let report = report(&mut session, "mail");
        let Some(Value::Object(members)) = report.get("members") else {
            panic!("members is {:?}", report.get("members"));
        };
        assert_eq!(members.keys().collect::<Vec<_>>(), vec!["carol"]);
        assert_eq!(
            value(
                &mut session,
                "ALTER GROUP 'mail' ON TOPIC events DROP MEMBER 'nobody';"
            ),
            Value::Number(Number::Integer(0))
        );
    });
}

#[test]
fn hand_needs_a_group_and_a_member_to_hand_to() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        mail_held_by_alice(&mut session, 1);
        for (script, expected) in [
            ("HAND events FOR CONSUMER 'mail' AT 1;", "`TO '<member>'`"),
            (
                "HAND events FOR CONSUMER 'solo' AT 1 TO 'bob';",
                "without a group",
            ),
            (
                "HAND events FOR CONSUMER 'mail' AT 1 TO 'bob' IF IDLE 'long';",
                "a duration",
            ),
        ] {
            let why = refused(&mut session, script);
            assert!(why.contains(expected), "{}: {script}: {why}", backend.name);
        }
    });
}
