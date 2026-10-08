//! Expiry fires no event (ADR-0122 A, restated as ADR-0124 D9).

use std::thread;
use std::time::Duration;

use super::{inside, rows, run, store};

#[test]
fn a_record_removed_by_expiry_runs_no_delete_event_and_a_deleted_one_does() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TABLE sessions (owner string) EXPIRE AFTER 50ms; DEFINE COLLECTION seen; \
         DEFINE EVENT gone ON sessions FOR DELETE THEN CREATE seen = { id: $id };",
    );
    run(
        &mut session,
        "CREATE sessions:1 = { owner: 'ada' }; CREATE sessions:2 = { owner: 'bob' };",
    );
    thread::sleep(Duration::from_millis(120));
    let removed = store.remove_expired().unwrap();
    assert_eq!(removed.records, 2, "{removed:?}");
    assert!(
        rows(&mut session, "SELECT * FROM seen;").is_empty(),
        "the pass ran no event"
    );

    run(
        &mut session,
        "DEFINE TABLE keep (owner string); CREATE keep:1 = { owner: 'c' }; \
         DEFINE EVENT gone ON keep FOR DELETE THEN CREATE seen = { id: $id }; DELETE keep:1;",
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM seen;").len(),
        1,
        "a DELETE does"
    );
}
