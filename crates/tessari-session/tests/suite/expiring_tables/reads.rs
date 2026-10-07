//! No read path answers a record of an expiring table once its instant has
//! passed, and a follower holds the same instant (ADR-0122 A7, A9; G069 C2).
//!
//! Every read is asked twice: before the instant, as the control that proves
//! the read can answer the record at all, and after it. A permanent record in
//! the same table is answered both times, so a read that simply stopped
//! answering anything cannot pass.

use std::sync::Arc;
use std::thread;
use std::time::Duration as Wait;

use tessari_kv::MemoryBackend;
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Sequence;

use super::{opened, run};

const PAST_SHORT: Wait = Wait::from_millis(450);

const SCHEMA: &str = "\
    DEFINE ANALYZER plain FILTERS lowercase, ascii; \
    DEFINE TABLE message (n int, email string, at vector<2>, doc object) EXPIRE AFTER 300ms; \
    DEFINE FIELD body ON message TYPE string ANALYZER plain; \
    DEFINE INDEX by_n ON message FIELDS n; \
    DEFINE INDEX by_email ON message FIELDS email UNIQUE; \
    DEFINE INDEX by_pair ON message FIELDS n, email; \
    DEFINE INDEX by_body ON message FIELDS body SEARCH; \
    DEFINE INDEX by_at ON message FIELDS at VECTOR euclidean; \
    DEFINE INDEX by_doc ON message FIELDS doc CONTAINS; \
    DEFINE TABLE pointer (target record); \
    CREATE message:1 = { n: 7, email: 'gone@x', body: 'a lock', at: [0.0, 0.0], doc: { k: 1 } }; \
    CREATE message:2 = { n: 7, email: 'kept@x', body: 'a lock', at: [0.1, 0.0], doc: { k: 1 } } EXPIRE NONE; \
    CREATE pointer:1 = { target: message:1 };";

/// The reads, each answering how many records it found.
const READS: &[&str] = &[
    "SELECT * FROM message;",
    "SELECT * FROM message:1;",
    "SELECT * FROM message WHERE n = 7;",
    "SELECT * FROM message WHERE email = 'gone@x';",
    "SELECT * FROM message WHERE n = 7 AND email = 'gone@x';",
    "SELECT * FROM message WHERE body MATCHES 'lock';",
    "SELECT * FROM message ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 5;",
    "SELECT * FROM message ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 1 APPROXIMATE;",
    "SELECT * FROM message WHERE doc CONTAINS { k: 1 };",
    "SELECT * FROM (SELECT * FROM message WHERE n = 7 LIMIT 10);",
    "SELECT count(*) AS n FROM message;",
];

fn answered(session: &mut Session<'_>, read: &str) -> String {
    format!("{:?}", run(session, read))
}

#[test]
fn every_read_path_answers_the_record_before_its_instant_and_none_after_it() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(&mut session, SCHEMA);
    let before_version = store.committed_version().unwrap();
    for read in READS {
        let held = answered(&mut session, read);
        assert!(
            held.contains("gone@x") || held.contains("Integer(2)") || read.contains("APPROXIMATE"),
            "control: {read} must answer the expiring record first: {held}"
        );
    }
    let fetched = answered(&mut session, "SELECT * FROM pointer FETCH target;");
    assert!(fetched.contains("gone@x"), "control: {fetched}");
    thread::sleep(PAST_SHORT);
    for read in READS {
        let held = answered(&mut session, read);
        assert!(
            !held.contains("gone@x"),
            "{read} answered an expired record: {held}"
        );
    }
    assert!(
        answered(&mut session, "SELECT count(*) AS n FROM message;").contains("Integer(1)"),
        "the count dropped to the permanent record"
    );
    assert!(
        answered(
            &mut session,
            "SELECT * FROM message WHERE email = 'kept@x';"
        )
        .contains("kept@x"),
        "the permanent record is still answered"
    );
    // A reference to an expired record stays a reference, as a deleted one's does.
    let fetched = answered(&mut session, "SELECT * FROM pointer FETCH target;");
    assert!(!fetched.contains("gone@x"), "{fetched}");
    // A read of the past does not bring an expired record back.
    let past = answered(
        &mut session,
        &format!("SELECT * FROM message VERSION {};", before_version.get()),
    );
    assert!(
        !past.contains("gone@x"),
        "VERSION answered an expired record: {past}"
    );
}

#[test]
fn an_index_and_a_scan_answer_the_same_records_after_the_instant() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(&mut session, SCHEMA);
    thread::sleep(PAST_SHORT);
    let ids = |session: &mut Session<'_>, read: &str| match run(session, read) {
        Outcome::Records { records, plan, .. } => (records, format!("{:?}", plan.access)),
        other => panic!("{read} answered {other:?}"),
    };
    let (by_index, index_path) = ids(
        &mut session,
        "SELECT id FROM message WHERE n = 7 USING INDEX by_n;",
    );
    let (by_scan, scan_path) = ids(&mut session, "SELECT id FROM message WHERE n + 0 = 7;");
    assert_eq!((index_path.as_str(), scan_path.as_str()), ("Index", "Scan"));
    assert_eq!(by_index.len(), 1, "only the permanent record: {by_index:?}");
    assert_eq!(by_index, by_scan, "the index and the scan disagree");
}

#[test]
fn a_follower_holds_the_leaders_instant_and_hides_the_record_after_it() {
    let leader = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let follower = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&leader);
    run(&mut session, SCHEMA);
    for log in leader.logs().unwrap() {
        for (sequence, record) in leader.log_records(log, Sequence::ZERO, 4096).unwrap() {
            follower
                .apply_record(log.writer, sequence, &record)
                .unwrap();
        }
    }
    let mut there = Session::new(&follower);
    run(&mut there, "USE NAMESPACE prod; USE DATABASE app;");
    assert!(format!("{:?}", run(&mut session, "RETURN TTL message:1;")).contains("Duration"));
    let left_here = format!("{:?}", run(&mut session, "RETURN TTL message:2;"));
    let left_there = format!("{:?}", run(&mut there, "RETURN TTL message:2;"));
    assert_eq!(
        left_here, left_there,
        "the permanent record is permanent on both"
    );
    assert!(answered(&mut there, "SELECT * FROM message;").contains("gone@x"));
    thread::sleep(PAST_SHORT);
    let after = answered(&mut there, "SELECT * FROM message;");
    assert!(
        !after.contains("gone@x"),
        "the follower answered an expired record: {after}"
    );
}

#[test]
fn the_removal_pass_removes_expired_table_records_and_says_how_many() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(&mut session, SCHEMA);
    thread::sleep(PAST_SHORT);
    let lapsed = store.remove_expired().unwrap();
    assert_eq!(lapsed.records, 1, "{lapsed:?}");
    assert_eq!(lapsed.stale, 0, "{lapsed:?}");
    assert_eq!(
        store.remove_expired().unwrap().records,
        0,
        "nothing is left to remove"
    );
}
