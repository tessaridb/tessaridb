//! S3.1 and S3.2: a reader's position, kept in the store and moved with the
//! reader's own writes.

use std::collections::BTreeMap;
use std::thread;

use tessari_types::{Number, RecordId, Value};

use super::super::key_value::{on_each_backend, run, value};
use super::{another, opened, read};

fn appended(session: &mut tessari_session::Session<'_>, count: u64) {
    for n in 1..=count {
        run(session, &format!("CREATE events:'m{n}' = {{ n: {n} }};"));
    }
}

fn positions(answered: &[(u64, RecordId)]) -> Vec<u64> {
    answered.iter().map(|(position, _)| *position).collect()
}

#[test]
fn a_position_moves_with_the_readers_commit_and_not_without_it() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TABLE done SCHEMALESS;");
        appended(&mut session, 5);
        // Read, write the result, and give both up: the next read is given the
        // same messages, and the result is not there.
        let given = |session: &mut tessari_session::Session<'_>, script: &str| {
            let outcomes = session.run(script).unwrap();
            let tessari_session::Outcome::Records { records, .. } = &outcomes[2] else {
                panic!("{outcomes:?}");
            };
            records
                .iter()
                .map(|(_, body)| {
                    let Value::Object(fields) = body else {
                        panic!()
                    };
                    let Some(Value::Number(Number::Integer(at))) = fields.get("position") else {
                        panic!()
                    };
                    u64::try_from(*at).unwrap()
                })
                .collect::<Vec<_>>()
        };
        let first = given(
            &mut session,
            "BEGIN; CREATE done:'first' = { ok: true }; \
             READ FROM events FOR CONSUMER 'billing' LIMIT 2; CANCEL;",
        );
        assert_eq!(first, vec![1, 2], "{}", backend.name);
        let again = given(
            &mut session,
            "BEGIN; CREATE done:'first' = { ok: true }; \
             READ FROM events FOR CONSUMER 'billing' LIMIT 2; COMMIT;",
        );
        assert_eq!(again, vec![1, 2], "{}: rolled back", backend.name);
        let (answered, _) = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'billing' LIMIT 2;",
        );
        assert_eq!(
            positions(&answered),
            vec![3, 4],
            "{}: committed",
            backend.name
        );
        let Value::Array(done) = value(&mut session, "RETURN (SELECT * FROM done);") else {
            panic!("no array");
        };
        assert_eq!(
            done.len(),
            1,
            "{}: the reader's write committed once",
            backend.name
        );
        // Another name has its own position, and AFTER moves one on purpose.
        let (answered, _) = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'audit' LIMIT 1;",
        );
        assert_eq!(positions(&answered), vec![1]);
        let (answered, _) = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'audit' AFTER 4;",
        );
        assert_eq!(positions(&answered), vec![5]);
        let (answered, _) = read(&mut session, "READ FROM events FOR CONSUMER 'audit';");
        assert!(answered.is_empty(), "{}", backend.name);
    });
}

#[test]
fn readers_of_one_name_are_given_every_message_exactly_once_between_them() {
    const MESSAGES: u64 = 60;
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        appended(&mut session, MESSAGES);
        let given: Vec<Vec<(u64, RecordId)>> = thread::scope(|scope| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let store = &backend.store;
                    scope.spawn(move || {
                        let mut session = another(store);
                        let mut mine = Vec::new();
                        // Bounded: a reader whose position never moves would
                        // otherwise spin here, and a hung test reports nothing.
                        for round in 0.. {
                            assert!(
                                round < 1_000,
                                "a reader was still being given messages after {round} reads"
                            );
                            match session.run("READ FROM events FOR CONSUMER 'workers' LIMIT 3;") {
                                Ok(mut outcomes) => {
                                    let outcome = outcomes.pop().unwrap();
                                    let tessari_session::Outcome::Records { records, .. } = outcome
                                    else {
                                        panic!("{outcome:?}");
                                    };
                                    if records.is_empty() {
                                        break;
                                    }
                                    mine.extend(records.into_iter().map(|(id, body)| {
                                        let Value::Object(fields) = body else {
                                            panic!()
                                        };
                                        let Some(Value::Number(Number::Integer(at))) =
                                            fields.get("position")
                                        else {
                                            panic!()
                                        };
                                        (u64::try_from(*at).unwrap(), id)
                                    }));
                                }
                                // A reader that lost its race past the re-run
                                // deadline reads again; nothing was given.
                                Err(why) if why.to_string().contains("conflict") => {}
                                Err(why) => panic!("{why}"),
                            }
                        }
                        mine
                    })
                })
                .collect();
            readers
                .into_iter()
                .map(|reader| reader.join().unwrap())
                .collect()
        });
        let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
        for (position, _) in given.iter().flatten() {
            *seen.entry(*position).or_default() += 1;
        }
        let twice: Vec<_> = seen.iter().filter(|(_, count)| **count > 1).collect();
        assert!(twice.is_empty(), "{}: given twice {twice:?}", backend.name);
        assert_eq!(
            seen.keys().copied().collect::<Vec<_>>(),
            (1..=MESSAGES).collect::<Vec<_>>(),
            "{}: every message given",
            backend.name
        );
    });
}

#[test]
fn info_for_topic_reports_positions_and_each_readers_lag() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        appended(&mut session, 7);
        let _ = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'billing' LIMIT 5;",
        );
        let _ = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'audit' LIMIT 1;",
        );
        let Value::Object(report) = value(&mut session, "INFO FOR TOPIC events;") else {
            panic!("no report");
        };
        let whole = |n: i64| Value::Number(Number::Integer(n));
        assert_eq!(report.get("first"), Some(&whole(1)), "{}", backend.name);
        assert_eq!(report.get("last"), Some(&whole(7)));
        let Some(Value::Object(readers)) = report.get("consumers") else {
            panic!("{report:?}");
        };
        let reader = |position: i64, lag: i64| {
            Value::Object(BTreeMap::from([
                ("position".to_owned(), whole(position)),
                ("lag".to_owned(), whole(lag)),
            ]))
        };
        assert_eq!(
            readers.get("billing"),
            Some(&reader(5, 2)),
            "{}",
            backend.name
        );
        assert_eq!(readers.get("audit"), Some(&reader(1, 6)));
        assert_eq!(readers.len(), 2);
    });
}
