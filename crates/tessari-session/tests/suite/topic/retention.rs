//! S4.1: retention removes messages through the log, keeps the numbering, and
//! tells a reader what it missed.

use std::thread;

use tessari_session::Note;

use super::super::key_value::{PAST_SHORT, SHORT, on_each_backend, run};
use super::{entries, opened, read};

fn missed(notes: &[Note]) -> Option<u64> {
    notes.iter().find_map(|note| match note {
        Note::Lapsed { missed, .. } => Some(*missed),
        _ => None,
    })
}

#[test]
fn retention_removes_messages_and_a_reader_behind_it_is_told_how_many_it_missed() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, &format!("DEFINE TOPIC short RETAIN {SHORT};"));
        for n in 1..=4 {
            run(
                &mut session,
                &format!("CREATE short:'m{n}' = {{ n: {n} }};"),
            );
        }
        let (answered, _) = read(&mut session, "READ FROM short FOR CONSUMER 'slow' LIMIT 1;");
        assert_eq!(answered.len(), 1, "{}", backend.name);
        thread::sleep(PAST_SHORT);

        // Passed but not yet removed: the read passes over them and says so.
        let (answered, notes) = read(&mut session, "READ FROM short AFTER 1;");
        assert!(answered.is_empty(), "{}", backend.name);
        assert_eq!(missed(&notes), Some(3), "{}: {notes:?}", backend.name);

        // Removed through the log: both indexes lose them, the numbering does not.
        let removed = backend.store.remove_expired().unwrap();
        assert_eq!(removed.records, 4, "{}: {removed:?}", backend.name);
        let (by_position, by_message) = entries(backend);
        assert!(
            by_position.is_empty() && by_message.is_empty(),
            "{}",
            backend.name
        );
        run(&mut session, "CREATE short:'m5' = { n: 5 };");
        let (answered, notes) = read(&mut session, "READ FROM short FOR CONSUMER 'slow';");
        let positions: Vec<u64> = answered.iter().map(|(position, _)| *position).collect();
        assert_eq!(positions, vec![5], "{}: numbering continued", backend.name);
        assert_eq!(missed(&notes), Some(3), "{}: {notes:?}", backend.name);

        // Told once: the reader's position moved past what it missed.
        let (_, notes) = read(&mut session, "READ FROM short FOR CONSUMER 'slow';");
        assert_eq!(missed(&notes), None, "{}", backend.name);
    });
}

#[test]
fn a_message_whose_retention_has_not_passed_is_not_removed_by_the_pass() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC long RETAIN 1h;");
        run(&mut session, "CREATE long:'kept' = { n: 1 };");
        let removed = backend.store.remove_expired().unwrap();
        assert_eq!(removed.records, 0, "{}", backend.name);
        let (answered, notes) = read(&mut session, "READ FROM long;");
        assert_eq!(answered.len(), 1, "{}", backend.name);
        assert!(notes.is_empty());
    });
}

/// The `bytes` a topic reports holding.
fn held_bytes(session: &mut tessari_session::Session<'_>, topic: &str) -> u64 {
    match run(session, &format!("INFO FOR TOPIC {topic};")) {
        tessari_session::Outcome::Value(tessari_types::Value::Object(report)) => {
            match report.get("bytes") {
                Some(tessari_types::Value::Number(tessari_types::Number::Integer(held))) => {
                    u64::try_from(*held).unwrap()
                }
                other => panic!("no bytes in {report:?}: {other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

/// `RETAIN BYTES n` keeps the newest messages that fit in n bytes: appending
/// past it removes the oldest, in the appending commit, and a reader behind
/// them is told how many it missed (G055 C8).
#[test]
fn a_size_retention_keeps_the_newest_messages_that_fit() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC sized RETAIN BYTES 120;");
        run(&mut session, "CREATE sized:'m1' = { body: 'aaaaaaaaaa' };");
        let one = held_bytes(&mut session, "sized");
        assert!(
            one > 0 && one <= 120,
            "{}: one message is {one} bytes",
            backend.name
        );
        let fit = 120 / one;
        for n in 2..=12 {
            run(
                &mut session,
                &format!("CREATE sized:'m{n}' = {{ body: 'aaaaaaaaaa' }};"),
            );
            let held = held_bytes(&mut session, "sized");
            assert!(held <= 120, "{}: {held} bytes held after {n}", backend.name);
        }
        let (answered, notes) = read(&mut session, "READ FROM sized FOR CONSUMER 'late';");
        let positions: Vec<u64> = answered.iter().map(|(position, _)| *position).collect();
        let kept = u64::try_from(positions.len()).unwrap();
        assert_eq!(kept, fit, "{}: {positions:?}", backend.name);
        // The newest, contiguous, ending at the last appended.
        assert_eq!(positions.last(), Some(&12), "{}", backend.name);
        assert_eq!(
            positions.first(),
            Some(&(13 - fit)),
            "{}: {positions:?}",
            backend.name
        );
        assert_eq!(
            missed(&notes),
            Some(12 - fit),
            "{}: {notes:?}",
            backend.name
        );
        // Both position indexes hold exactly what is kept.
        let (by_position, by_message) = entries(backend);
        assert_eq!(by_position.len(), positions.len(), "{}", backend.name);
        assert_eq!(by_message.len(), positions.len(), "{}", backend.name);
        assert_eq!(
            held_bytes(&mut session, "sized"),
            kept * one,
            "{}",
            backend.name
        );
    });
}

#[test]
fn a_message_larger_than_the_size_retention_is_refused() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC tiny RETAIN BYTES 8;");
        let refused = session
            .run("CREATE tiny:'big' = { body: 'far more than eight bytes' };")
            .unwrap_err();
        assert!(
            matches!(
                refused,
                tessari_session::Error::Store(tessari_storage::Error::TopicMessageTooLarge {
                    max: 8,
                    ..
                })
            ),
            "{}: {refused:?}",
            backend.name
        );
    });
}

/// A commit whose own messages do not fit is refused rather than trimmed: a
/// message that vanished on commit is the loss this limit must not produce.
#[test]
fn a_commit_that_alone_outgrows_the_size_retention_is_refused() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC small RETAIN BYTES 60;");
        let mut script = String::from("BEGIN;");
        for n in 1..=10 {
            script.push_str(&format!(" CREATE small:'m{n}' = {{ body: 'aaaaaaaaaa' }};"));
        }
        script.push_str(" COMMIT;");
        let refused = session.run(&script).unwrap_err();
        assert!(
            matches!(
                refused,
                tessari_session::Error::Store(tessari_storage::Error::TopicRetainExceeded { .. })
            ),
            "{}: {refused:?}",
            backend.name
        );
        let (answered, _) = read(&mut session, "READ FROM small;");
        assert!(answered.is_empty(), "{}", backend.name);
    });
}

/// The count a size retention is enforced against is recounted when it is not
/// there — a store restored without it enforces the same limit.
#[test]
fn a_size_retention_recounts_what_it_holds_when_the_count_is_gone() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC counted RETAIN BYTES 120;");
        for n in 1..=3 {
            run(
                &mut session,
                &format!("CREATE counted:'m{n}' = {{ body: 'aaaaaaaaaa' }};"),
            );
        }
        let before = held_bytes(&mut session, "counted");
        let kind = tessari_encoding::KeyKind::TopicBytes;
        let keys: Vec<_> = backend
            .raw
            .scan(&tessari_kv::ScanRequest {
                keyspace: kind.keyspace(),
                range: tessari_kv::KeyRange::prefix(&[kind.tag()]),
                direction: tessari_kv::ScanDirection::Forward,
                limit: None,
            })
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys.len(), 1, "{}: the count is kept", backend.name);
        let mut batch = tessari_kv::WriteBatch::default();
        for key in keys {
            batch = batch.delete(kind.keyspace(), key);
        }
        backend.raw.apply(batch).unwrap();
        for n in 4..=12 {
            run(
                &mut session,
                &format!("CREATE counted:'m{n}' = {{ body: 'aaaaaaaaaa' }};"),
            );
        }
        let after = held_bytes(&mut session, "counted");
        assert!(after <= 120 && after >= before, "{}: {after}", backend.name);
    });
}
