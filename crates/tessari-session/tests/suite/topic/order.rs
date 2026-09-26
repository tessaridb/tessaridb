//! S2.1 and S2.2: dense positions in commit order, and reading after one.

use std::collections::BTreeSet;
use std::thread;

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
                                .run(&format!("CREATE events:'w{writer}n{n}' = {{ n: {n} }};"))
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
                        let _ =
                            session.run(&format!("CREATE events:'w{writer}n{n}' = {{ n: {n} }};"));
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
