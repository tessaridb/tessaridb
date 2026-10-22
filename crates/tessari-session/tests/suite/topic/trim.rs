//! `TRIM` removes a topic's oldest messages, and only those (ADR-0133, W8c).
//!
//! A topic's positions stay dense from the first message it holds, so a
//! reader behind the trim is told how many it missed rather than meeting a
//! hole. A trim is the one way a caller removes a message: `DELETE` still
//! refuses, which `declaring.rs` holds.

use tessari_session::Note;
use tessari_types::{Number, RecordId, Value};

use super::super::key_value::{on_each_backend, refused, run, value};
use super::{entries, opened, read};

fn append(session: &mut tessari_session::Session<'_>, ids: &[&str]) {
    for id in ids {
        run(
            session,
            &format!("CREATE events:'{id}' = {{ id: '{id}' }};"),
        );
    }
}

fn positions(session: &mut tessari_session::Session<'_>) -> Vec<(u64, RecordId)> {
    read(session, "READ FROM events;").0
}

fn named(position: u64, id: &str) -> (u64, RecordId) {
    (position, RecordId::Text(id.to_owned()))
}

fn removed(session: &mut tessari_session::Session<'_>, script: &str) -> i64 {
    match value(session, script) {
        Value::Number(Number::Integer(count)) => count,
        other => panic!("{script} answered {other:?}"),
    }
}

#[test]
fn keep_removes_all_but_the_newest_messages_and_numbering_carries_on() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        append(&mut session, &["f", "e", "d", "c", "b", "a"]);
        assert_eq!(
            removed(&mut session, "TRIM events KEEP 2;"),
            4,
            "{}",
            backend.name
        );
        assert_eq!(positions(&mut session), vec![named(5, "b"), named(6, "a")]);
        append(&mut session, &["g"]);
        let (by_position, by_message) = entries(backend);
        assert_eq!(
            by_position,
            vec![named(5, "b"), named(6, "a"), named(7, "g")],
            "{}",
            backend.name
        );
        assert_eq!(by_message.len(), 3, "{}: {by_message:?}", backend.name);
        assert_eq!(removed(&mut session, "TRIM events KEEP 5;"), 0);
        assert_eq!(removed(&mut session, "TRIM events KEEP 0;"), 3);
        assert!(positions(&mut session).is_empty());
        append(&mut session, &["h"]);
        assert_eq!(positions(&mut session), vec![named(8, "h")]);
    });
}

#[test]
fn before_removes_the_messages_below_a_position_and_a_reader_behind_is_told() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        append(&mut session, &["d", "c", "b", "a"]);
        let (first, _) = read(
            &mut session,
            "READ FROM events FOR CONSUMER 'slow' LIMIT 1;",
        );
        assert_eq!(first, vec![named(1, "d")]);
        assert_eq!(
            removed(&mut session, "TRIM events BEFORE 3;"),
            2,
            "{}",
            backend.name
        );
        assert_eq!(removed(&mut session, "TRIM events BEFORE 3;"), 0);
        let (next, notes) = read(&mut session, "READ FROM events FOR CONSUMER 'slow';");
        assert_eq!(next, vec![named(3, "b"), named(4, "a")], "{}", backend.name);
        let missed = notes.iter().find_map(|note| match note {
            Note::Lapsed { missed, .. } => Some(*missed),
            _ => None,
        });
        assert_eq!(missed, Some(1), "{}: {notes:?}", backend.name);
        assert_eq!(removed(&mut session, "TRIM events BEFORE 100;"), 2);
        append(&mut session, &["e"]);
        assert_eq!(positions(&mut session), vec![named(5, "e")]);
    });
}

#[test]
fn a_trim_is_refused_on_what_is_not_a_topic_and_for_a_count_that_is_not_one() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TABLE plain SCHEMALESS;");
        for (script, expected) in [
            ("TRIM plain KEEP 1;", "is not a topic"),
            ("TRIM events KEEP -1;", "a whole number at or above zero"),
            ("TRIM events BEFORE 'x';", "a whole number at or above zero"),
            ("TRIM events;", "`KEEP n` or `BEFORE p`"),
        ] {
            let why = refused(&mut session, script);
            assert!(why.contains(expected), "{}: {script}: {why}", backend.name);
        }
    });
}

#[test]
fn a_trim_inside_a_transaction_is_undone_with_it() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        append(&mut session, &["b", "a"]);
        run(&mut session, "BEGIN; TRIM events KEEP 0; CANCEL;");
        assert_eq!(positions(&mut session), vec![named(1, "b"), named(2, "a")]);
    });
}
