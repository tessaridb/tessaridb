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
