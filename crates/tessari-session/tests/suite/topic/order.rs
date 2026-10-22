//! S2.1 and S2.2: dense positions in commit order, and reading after one.

use std::collections::BTreeSet;
use std::thread;
use tessari_session::RunNow;

use tessari_types::RecordId;

use super::super::key_value::{on_each_backend, refused, run};
use super::{another, entries, opened, read};

#[test]
fn positions_are_dense_from_one_in_commit_order_and_both_indexes_agree() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "CREATE events:'c' = { n: 1 }; CREATE events:'a' = { n: 2 };",
        );
        run(
            &mut session,
            "BEGIN; CREATE events:'z' = { n: 3 }; CREATE events:'b' = { n: 4 }; COMMIT;",
        );
        run(&mut session, "CREATE events:'m' = { n: 5 };");
        let expected: Vec<(u64, RecordId)> = ["c", "a", "z", "b", "m"]
            .iter()
            .zip(1_u64..)
            .map(|(id, position)| (position, RecordId::from(*id)))
            .collect();
        let (answered, notes) = read(&mut session, "READ FROM events;");
        assert_eq!(answered[..2], expected[..2], "{}", backend.name);
        // Within one commit the order is the commit's own; across commits it is
        // the commit order.
        let middle: BTreeSet<_> = answered[2..4].iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(
            middle,
            BTreeSet::from([RecordId::from("z"), RecordId::from("b")])
        );
        assert_eq!(answered[4], expected[4], "{}", backend.name);
        assert!(notes.is_empty(), "{notes:?}");
        let (by_position, by_message) = entries(backend);
        assert_eq!(
            by_position, answered,
            "{}: the position index",
            backend.name
        );
        let reversed: Vec<(u64, RecordId)> = by_message
            .into_iter()
            .map(|(id, position)| (position, id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(reversed, answered, "{}: the identity index", backend.name);
    });
}

#[test]
fn concurrent_appenders_leave_no_gap_and_no_position_twice() {
    const WRITERS: usize = 4;
    const EACH: usize = 40;
    on_each_backend(|backend| {
        let _ = opened(&backend.store);
        thread::scope(|scope| {
            for writer in 0..WRITERS {
                let store = &backend.store;
                scope.spawn(move || {
                    let mut session = another(store);
                    for n in 0..EACH {
                        // Bounded, so an append that can never land fails by name.
                        for attempt in 0.. {
                            assert!(
                                attempt < 1_000,
                                "w{writer}n{n} was still refused after {attempt} attempts"
                            );
                            match session
                                .run_now(&format!("CREATE events:'w{writer}n{n}' = {{ n: {n} }};"))
                            {
                                Ok(_) => break,
                                Err(why) if why.to_string().contains("gave up") => {}
                                Err(why) => panic!("{why}"),
                            }
                        }
                    }
                });
            }
        });
        let written: BTreeSet<RecordId> = (0..WRITERS)
            .flat_map(|writer| (0..EACH).map(move |n| RecordId::from(format!("w{writer}n{n}"))))
            .collect();
        let (by_position, by_message) = entries(backend);
        let positions: Vec<u64> = by_position.iter().map(|(position, _)| *position).collect();
        let dense: Vec<u64> = (1..=u64::try_from(written.len()).unwrap()).collect();
        assert_eq!(positions, dense, "{}: positions", backend.name);
        let filed: Vec<RecordId> = by_position.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(
            filed.len(),
            written.len(),
            "{}: a message filed twice",
            backend.name
        );
        assert_eq!(filed.iter().cloned().collect::<BTreeSet<_>>(), written);
        assert_eq!(by_message.keys().cloned().collect::<BTreeSet<_>>(), written);
        for (position, id) in &by_position {
            assert_eq!(
                by_message.get(id),
                Some(position),
                "{}: {id:?}",
                backend.name
            );
        }
    });
}

#[test]
fn a_reader_beside_appenders_only_ever_sees_a_contiguous_prefix() {
    on_each_backend(|backend| {
        let _ = opened(&backend.store);
        thread::scope(|scope| {
            for writer in 0..3 {
                let store = &backend.store;
                scope.spawn(move || {
                    let mut session = another(store);
                    for n in 0..30 {
                        let _ = session
                            .run_now(&format!("CREATE events:'w{writer}n{n}' = {{ n: {n} }};"));
                    }
                });
            }
            let mut reader = another(&backend.store);
            for _ in 0..40 {
                let (answered, _) = read(&mut reader, "READ FROM events LIMIT 1000;");
                let positions: Vec<u64> = answered.iter().map(|(position, _)| *position).collect();
                let prefix: Vec<u64> = (1..=u64::try_from(positions.len()).unwrap()).collect();
                assert_eq!(
                    positions, prefix,
                    "{}: a read skipped a position",
                    backend.name
                );
            }
        });
    });
}

#[test]
fn after_and_limit_bound_the_read_and_a_plain_table_is_not_a_topic() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        for n in 1..=6 {
            run(
                &mut session,
                &format!("CREATE events:'m{n}' = {{ n: {n} }};"),
            );
        }
        let (answered, _) = read(&mut session, "READ FROM events AFTER 2 LIMIT 3;");
        let positions: Vec<u64> = answered.iter().map(|(position, _)| *position).collect();
        assert_eq!(positions, vec![3, 4, 5], "{}", backend.name);
        let (answered, _) = read(&mut session, "READ FROM events AFTER 6;");
        assert!(answered.is_empty());
        run(&mut session, "DEFINE TABLE plain SCHEMALESS;");
        let why = refused(&mut session, "READ FROM plain;");
        assert!(why.contains("plain is not a topic"), "{why}");
        let why = refused(&mut session, "READ FROM events AFTER -1;");
        assert!(why.contains("whole number"), "{why}");
    });
}

/// The positions a lone append answers, as integers in the order of its rows.
fn positions(session: &mut tessari_session::Session<'_>, script: &str) -> Vec<i64> {
    match super::super::key_value::value(session, script) {
        tessari_types::Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                tessari_types::Value::Number(tessari_types::Number::Integer(held)) => held,
                other => panic!("{script} answered a position of {other:?}"),
            })
            .collect(),
        other => panic!("{script} answered {other:?}"),
    }
}

#[test]
fn a_lone_append_answers_the_positions_its_messages_took() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            positions(&mut session, "CREATE events = { n: 1 } RETURN POSITION;"),
            [1]
        );
        assert_eq!(
            positions(
                &mut session,
                "INSERT INTO events (n) VALUES (2), (3), (4) RETURN POSITION;"
            ),
            [2, 3, 4],
            "{}",
            backend.name
        );
        assert_eq!(
            positions(
                &mut session,
                "CREATE events:'named' = { n: 5 } RETURN POSITION;"
            ),
            [5]
        );
        // What READ FROM answers for the same messages, so the two agree.
        let (answered, _) = read(&mut session, "READ FROM events AFTER 1 LIMIT 3;");
        let read_positions: Vec<u64> = answered.iter().map(|(position, _)| *position).collect();
        assert_eq!(read_positions, [2, 3, 4]);
    });
}

#[test]
fn a_position_is_refused_where_none_is_decided_or_none_exists() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE COLLECTION notes;");
        let inside = refused(
            &mut session,
            "BEGIN; CREATE events = { n: 1 } RETURN POSITION; COMMIT;",
        );
        assert!(
            inside.contains("decided when the message commits"),
            "{inside}"
        );
        let elsewhere = refused(
            &mut session,
            "INSERT INTO notes (n) VALUES (1) RETURN POSITION;",
        );
        assert!(elsewhere.contains("is not a topic"), "{elsewhere}");
        let verb = refused(&mut session, "UPDATE events:1 = { n: 2 } RETURN POSITION;");
        assert!(
            verb.contains("only a create or an insert takes a position"),
            "{verb}"
        );
        // Nothing the refused transaction or the refused insert wrote landed.
        let (answered, _) = read(&mut session, "READ FROM events;");
        assert!(answered.is_empty(), "{answered:?}");
    });
}

/// Each message's `appended` instant, in the order answered.
fn appended(session: &mut tessari_session::Session<'_>, script: &str) -> Vec<(u64, i64)> {
    match run(session, script) {
        tessari_session::Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, body)| {
                let tessari_types::Value::Object(fields) = body else {
                    panic!("a message answered {body:?}");
                };
                let Some(tessari_types::Value::Number(tessari_types::Number::Integer(position))) =
                    fields.get("position")
                else {
                    panic!("no position in {fields:?}");
                };
                let Some(tessari_types::Value::Datetime(at)) = fields.get("appended") else {
                    panic!("no appended instant in {fields:?}");
                };
                let millis = at
                    .seconds()
                    .saturating_mul(1000)
                    .saturating_add(i64::from(at.nanos() / 1_000_000));
                (u64::try_from(*position).unwrap(), millis)
            })
            .collect(),
        other => panic!("{script} answered {other:?}"),
    }
}

fn wall_millis() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A message answers the instant it was appended, and `SINCE` starts a read
/// at the first message appended at or after an instant (ADR-0133 D3).
#[test]
fn a_message_answers_when_it_was_appended_and_since_reads_from_there() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let before = wall_millis();
        run(&mut session, "CREATE events:'a' = { n: 1 };");
        let first = wall_millis();
        while wall_millis() <= first {
            std::hint::spin_loop();
        }
        run(
            &mut session,
            "BEGIN; CREATE events:'b' = { n: 2 }; CREATE events:'c' = { n: 3 }; COMMIT;",
        );
        let answered = appended(&mut session, "READ FROM events;");
        assert_eq!(answered.len(), 3, "{}", backend.name);
        assert!(
            before <= answered[0].1 && answered[0].1 <= first,
            "{answered:?}"
        );
        assert!(answered[1].1 > first, "{answered:?}");
        assert_eq!(answered[1].1, answered[2].1, "one commit, one instant");
        let since = |at: i64| format!("datetime '1970-01-01T00:00:00Z' + {at}ms");
        let (from, _) = read(
            &mut session,
            &format!("READ FROM events SINCE {};", since(answered[1].1)),
        );
        let positions: Vec<u64> = from.iter().map(|(position, _)| *position).collect();
        assert_eq!(positions, vec![2, 3], "{}", backend.name);
        let (from, _) = read(
            &mut session,
            &format!("READ FROM events SINCE {} LIMIT 1;", since(0)),
        );
        assert_eq!(from.len(), 1);
        assert_eq!(from[0].0, 1);
        let (from, _) = read(
            &mut session,
            &format!(
                "READ FROM events SINCE {};",
                since(answered[2].1.saturating_add(1))
            ),
        );
        assert!(from.is_empty(), "{from:?}");
        // A named reader starting from an instant moves its position there,
        // as AFTER moves it.
        let (from, _) = read(
            &mut session,
            &format!(
                "READ FROM events FOR CONSUMER 'audit' SINCE {} LIMIT 1;",
                since(answered[1].1)
            ),
        );
        assert_eq!(from[0].0, 2);
        let (next, _) = read(&mut session, "READ FROM events FOR CONSUMER 'audit';");
        assert_eq!(next[0].0, 3, "{}", backend.name);
        let why = refused(&mut session, "READ FROM events SINCE 5;");
        assert!(why.contains("an instant is a datetime"), "{why}");
        let why = refused(&mut session, "READ FROM events AFTER 1 SINCE time::now();");
        assert!(!why.is_empty());
    });
}
