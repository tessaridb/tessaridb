//! G042 C2: a consumer group hands each message to one member and forgets it
//! only when it is acknowledged (ADR-0086).

use std::time::Duration;

use tessari_session::{Error, Outcome, Session};
use tessari_types::{Number, Value};

use super::super::key_value::{on_each_backend, run, value};
use super::{opened, read};

fn appended(session: &mut Session<'_>, count: u64) {
    for n in 1..=count {
        run(session, &format!("CREATE events:'m{n}' = {{ n: {n} }};"));
    }
}

/// Each answered message's position and how many times it has been handed out.
fn given(session: &mut Session<'_>, script: &str) -> Vec<(u64, u64)> {
    match run(session, script) {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, body)| {
                let Value::Object(fields) = body else {
                    panic!("a message answered {body:?}");
                };
                let whole = |field: &str| match fields.get(field) {
                    Some(Value::Number(Number::Integer(held))) => u64::try_from(*held).unwrap(),
                    other => panic!("{field} is {other:?}"),
                };
                (whole("position"), whole("deliveries"))
            })
            .collect(),
        other => panic!("{script} answered {other:?}"),
    }
}

fn count(session: &mut Session<'_>, script: &str) -> i64 {
    match value(session, script) {
        Value::Number(Number::Integer(held)) => held,
        other => panic!("{script} answered {other:?}"),
    }
}

/// One field of a group's report in `INFO FOR TOPIC`.
fn reported(session: &mut Session<'_>, group: &str, field: &str) -> Value {
    let Value::Object(report) = value(session, "INFO FOR TOPIC events;") else {
        panic!("INFO FOR TOPIC answered no object");
    };
    let Some(Value::Object(groups)) = report.get("groups") else {
        panic!("no groups in {report:?}");
    };
    let Some(Value::Object(fields)) = groups.get(group) else {
        panic!("no group {group} in {groups:?}");
    };
    fields.get(field).cloned().unwrap_or(Value::None)
}

const READ: &str = "READ FROM events FOR CONSUMER 'billing' LIMIT 10;";

#[test]
fn an_acknowledged_message_is_never_handed_out_again() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s IN FLIGHT 2;",
        );
        appended(&mut session, 3);
        assert_eq!(
            given(&mut session, READ),
            vec![(1, 1), (2, 1)],
            "{}",
            backend.name
        );
        // The group holds as many as it may: nothing more until one is done.
        assert_eq!(given(&mut session, READ), vec![], "{}", backend.name);
        assert_eq!(
            count(&mut session, "ACK events FOR CONSUMER 'billing' AT 1;"),
            1
        );
        assert_eq!(given(&mut session, READ), vec![(3, 1)], "{}", backend.name);
        // Acknowledging again changes nothing and refuses nothing.
        assert_eq!(
            count(&mut session, "ACK events FOR CONSUMER 'billing' AT 1;"),
            0
        );
        assert_eq!(
            count(&mut session, "ACK events FOR CONSUMER 'billing' AT 2, 3;"),
            2
        );
        assert_eq!(given(&mut session, READ), vec![], "{}", backend.name);
        assert_eq!(
            reported(&mut session, "billing", "committed"),
            Value::Number(Number::Integer(3))
        );
    });
}

#[test]
fn an_unacknowledged_message_is_handed_out_again_after_its_deadline() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 50ms;",
        );
        appended(&mut session, 2);
        assert_eq!(given(&mut session, READ), vec![(1, 1)], "{}", backend.name);
        // One in flight is the default width, and it keeps the order: the second
        // message waits for the first.
        assert_eq!(given(&mut session, READ), vec![], "{}", backend.name);
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(given(&mut session, READ), vec![(1, 2)], "{}", backend.name);
        assert_eq!(
            reported(&mut session, "billing", "redelivered"),
            Value::Number(Number::Integer(1))
        );
    });
}

#[test]
fn a_negative_acknowledgement_hands_the_message_out_again_at_once() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s;",
        );
        appended(&mut session, 2);
        assert_eq!(given(&mut session, READ), vec![(1, 1)]);
        assert_eq!(
            count(&mut session, "NACK events FOR CONSUMER 'billing' AT 1;"),
            1
        );
        assert_eq!(given(&mut session, READ), vec![(1, 2)], "{}", backend.name);
        // With a delay it waits for the delay instead.
        assert_eq!(
            count(
                &mut session,
                "NACK events FOR CONSUMER 'billing' AT 1 DELAY 30s;"
            ),
            1
        );
        assert_eq!(given(&mut session, READ), vec![], "{}", backend.name);
    });
}

#[test]
fn past_its_deliveries_a_message_goes_to_the_dead_letter_and_the_group_moves_on() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC dead;\n\
             DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s DELIVERIES 2 \
             DEAD LETTER TO dead;",
        );
        appended(&mut session, 2);
        assert_eq!(given(&mut session, READ), vec![(1, 1)]);
        run(&mut session, "NACK events FOR CONSUMER 'billing' AT 1;");
        assert_eq!(given(&mut session, READ), vec![(1, 2)]);
        run(&mut session, "NACK events FOR CONSUMER 'billing' AT 1;");
        // Delivered as often as the group allows: dead-lettered, not handed out,
        // and the room it held goes to the next message.
        assert_eq!(given(&mut session, READ), vec![(2, 1)], "{}", backend.name);
        assert_eq!(
            reported(&mut session, "billing", "dead_lettered"),
            Value::Number(Number::Integer(1))
        );
        let Outcome::Records { records, .. } = run(&mut session, "READ FROM dead;") else {
            panic!("the dead letter answered no records");
        };
        assert_eq!(records.len(), 1, "{}", backend.name);
        let Value::Object(message) = &records[0].1 else {
            panic!()
        };
        let Some(Value::Object(letter)) = message.get("value") else {
            panic!("{message:?}")
        };
        assert_eq!(letter.get("group"), Some(&Value::from("billing")));
        assert_eq!(
            letter.get("position"),
            Some(&Value::Number(Number::Integer(1)))
        );
        assert_eq!(
            letter.get("deliveries"),
            Some(&Value::Number(Number::Integer(2)))
        );
    });
}

#[test]
fn a_group_moved_by_alter_starts_after_the_position_it_was_given() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        appended(&mut session, 4);
        // A reader that read before the group existed: the group takes over its
        // position rather than handing it everything again.
        let (answered, _) = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'billing' LIMIT 2;",
        );
        assert_eq!(answered.len(), 2);
        run(
            &mut session,
            "DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s;",
        );
        assert_eq!(given(&mut session, READ), vec![(3, 1)], "{}", backend.name);
        run(
            &mut session,
            "ALTER GROUP 'billing' ON TOPIC events START AT 0;",
        );
        assert_eq!(given(&mut session, READ), vec![(1, 1)], "{}", backend.name);
    });
}

#[test]
fn a_group_is_refused_what_it_cannot_mean() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TABLE plain SCHEMALESS;\n\
             DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s;",
        );
        let refusal = |session: &mut Session<'_>, script: &str| match session.run(script) {
            Err(why) => why,
            Ok(outcome) => panic!("{script} answered {outcome:?}"),
        };
        assert!(matches!(
            refusal(&mut session, "ACK events FOR CONSUMER 'nobody' AT 1;"),
            Error::NotAGroup { .. }
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "READ FROM events FOR CONSUMER 'billing' AFTER 3;"
            ),
            Error::AfterOnGroup { .. }
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s;"
            ),
            Error::GroupExists { .. }
        ));
        run(
            &mut session,
            "DEFINE GROUP IF NOT EXISTS 'billing' ON TOPIC events ACK DEADLINE 1s;",
        );
        assert!(matches!(
            refusal(&mut session, "DROP GROUP 'audit' ON TOPIC events;"),
            Error::NoSuchGroup { .. }
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "DEFINE GROUP 'loop' ON TOPIC events ACK DEADLINE 30s DELIVERIES 3 \
                 DEAD LETTER TO events;"
            ),
            Error::DeadLetterIsTheTopic { .. }
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "DEFINE GROUP 'x' ON TOPIC plain ACK DEADLINE 30s;"
            ),
            Error::NotATopic { .. }
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "DEFINE GROUP 'x' ON TOPIC events ACK DEADLINE 30s DEAD LETTER TO events;"
            ),
            Error::Script(_)
        ));
        assert!(matches!(
            refusal(
                &mut session,
                "DEFINE GROUP 'x' ON TOPIC events IN FLIGHT 2;"
            ),
            Error::Script(_)
        ));
        run(&mut session, "DROP GROUP 'billing' ON TOPIC events;");
        assert!(matches!(
            refusal(&mut session, "ACK events FOR CONSUMER 'billing' AT 1;"),
            Error::NotAGroup { .. }
        ));
        let _ = backend;
    });
}

/// Four members share `messages` messages: every one is handed out once and
/// acknowledged once, and none is lost (G042 C4).
fn members_share_every_message_once(store: &tessari_storage::Store, messages: u64, name: &str) {
    let mut session = super::another(store);
    run(
        &mut session,
        "DEFINE GROUP IF NOT EXISTS 'workers' ON TOPIC events ACK DEADLINE 30s IN FLIGHT 50;",
    );
    appended(&mut session, messages);
    let handled: Vec<Vec<u64>> =
        std::thread::scope(|scope| {
            let members: Vec<_> =
                (0..4)
                    .map(|_| {
                        scope.spawn(move || {
                    let mut session = super::another(store);
                    let mut mine = Vec::new();
                    for round in 0.. {
                        assert!(round < 100_000, "a member was still reading after {round} reads");
                        let taken = match session
                            .run("READ FROM events FOR CONSUMER 'workers' LIMIT 10;")
                        {
                            Ok(mut outcomes) => match outcomes.pop().unwrap() {
                                Outcome::Records { records, .. } => records
                                    .into_iter()
                                    .map(|(_, body)| {
                                        let Value::Object(fields) = body else { panic!() };
                                        let Some(Value::Number(Number::Integer(at))) =
                                            fields.get("position")
                                        else {
                                            panic!()
                                        };
                                        u64::try_from(*at).unwrap()
                                    })
                                    .collect::<Vec<_>>(),
                                other => panic!("{other:?}"),
                            },
                            Err(why) if why.to_string().contains("conflict") => continue,
                            Err(why) => panic!("{why}"),
                        };
                        if taken.is_empty() {
                            break;
                        }
                        let list = taken
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        loop {
                            match session
                                .run(&format!("ACK events FOR CONSUMER 'workers' AT {list};"))
                            {
                                Ok(mut outcomes) => {
                                    let settled = match outcomes.pop().unwrap() {
                                        Outcome::Value(Value::Number(Number::Integer(n))) => n,
                                        other => panic!("{other:?}"),
                                    };
                                    assert_eq!(
                                        usize::try_from(settled).unwrap(),
                                        taken.len(),
                                        "an acknowledgement settled what another member held"
                                    );
                                    break;
                                }
                                Err(why) if why.to_string().contains("conflict") => {}
                                Err(why) => panic!("{why}"),
                            }
                        }
                        mine.extend(taken);
                    }
                    mine
                })
                    })
                    .collect();
            members
                .into_iter()
                .map(|member| member.join().unwrap())
                .collect()
        });
    let mut seen = std::collections::BTreeMap::<u64, usize>::new();
    for position in handled.iter().flatten() {
        let handled = seen.entry(*position).or_default();
        *handled = handled.saturating_add(1);
    }
    let twice: Vec<_> = seen.iter().filter(|(_, count)| **count > 1).collect();
    assert!(twice.is_empty(), "{name}: handled twice {twice:?}");
    assert_eq!(
        seen.keys().copied().collect::<Vec<_>>(),
        (1..=messages).collect::<Vec<_>>(),
        "{name}: every message handled"
    );
    assert_eq!(
        reported(&mut session, "workers", "in_flight"),
        Value::Number(Number::Integer(0)),
        "{name}"
    );
}

#[test]
fn members_of_one_group_share_the_work_with_nothing_lost_or_doubled() {
    on_each_backend(|backend| {
        let _ = opened(&backend.store);
        members_share_every_message_once(&backend.store, 200, backend.name);
    });
}

/// The C4 measurement: a thousand messages, four members, twenty runs. Run it
/// alone and under deliberate CPU load; it is ignored in the suite for its
/// length, not for its kind.
#[test]
#[ignore = "G042 C4 evidence: 20 runs × 1 000 messages; run with --ignored under CPU load"]
fn twenty_runs_of_a_thousand_messages_between_four_members() {
    for run_number in 1..=20 {
        on_each_backend(|backend| {
            let _ = opened(&backend.store);
            members_share_every_message_once(
                &backend.store,
                1_000,
                &format!("{} run {run_number}", backend.name),
            );
        });
    }
}
