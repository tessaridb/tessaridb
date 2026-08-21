//! A restored store is the store.
//!
//! The backup is the log, because state is a pure function of it. So a restore
//! that produced anything different would mean something in this store is *not*
//! derived — a defect in the store rather than in the backup. That makes this
//! file the strongest statement available anywhere in the repository that the
//! central claim is true, and it is why the fixture exercises every engine
//! rather than a table of two rows.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use bgv_db_session::Session;
use bgv_db_storage::Store;

/// Every engine this project has built, in one script.
///
/// Records, an ordered index, a unique one, a path index, a full-text index with
/// its maintained statistics, a vector graph, edges, a key-value space, schema
/// declarations with a default, users — and updates and deletes after the
/// indexes exist, so the maintenance path runs rather than only the build path.
const EVERYTHING: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE ANALYZER simple FILTERS lowercase, ascii;\n\
DEFINE TABLE people;\n\
DEFINE FIELD name ON people TYPE string;\n\
DEFINE FIELD bio ON people TYPE string ANALYZER simple;\n\
DEFINE FIELD joined ON people TYPE datetime DEFAULT time::now();\n\
DEFINE INDEX by_email ON people FIELDS email UNIQUE;\n\
DEFINE INDEX by_city ON people FIELDS address.city;\n\
DEFINE INDEX by_bio ON people FIELDS bio SEARCH;\n\
DEFINE INDEX by_at ON people FIELDS at VECTOR euclidean;\n\
CREATE people:1 = { name: 'ada', email: 'a@x', bio: 'lock contention on the write path', \
address: { city: 'london' }, at: [0.0, 0.0] };\n\
CREATE people:2 = { name: 'grace', email: 'b@x', bio: 'a compiler and a lock', \
address: { city: 'york' }, at: [1.0, 0.0] };\n\
CREATE people:3 = { name: 'edith', email: 'c@x', bio: 'nothing about locks here', \
address: { city: 'london' }, at: [5.0, 0.0] };\n\
UPDATE people:2 = { name: 'grace hopper', email: 'b@x', bio: 'a compiler', \
address: { city: 'york' }, at: [2.0, 0.0] };\n\
CREATE people:4 = { name: 'transient', email: 'd@x', bio: 'gone', at: [9.0, 0.0] };\n\
DELETE people:4;\n\
DEFINE TABLE follows EDGE;\n\
RELATE people:1->follows->people:2;\n\
RELATE people:2->follows->people:3;\n\
DEFINE TABLE sessions;\n\
SET sessions:'abc' = { user: people:1, level: 3 };\n\
SET sessions:'def' = [1, 2, 3];\n\
DEL sessions:'def';\n\
DEFINE USER root ROLE owner PASSWORD 'a long one';";

/// Reads that touch each engine, so a difference anywhere shows up as an answer.
const INTERROGATION: &[&str] = &[
    "SELECT * FROM people;",
    "SELECT * FROM people WHERE email = 'b@x';",
    "SELECT * FROM people WHERE address.city = 'london';",
    "SELECT * FROM people WHERE bio MATCHES 'lock';",
    "SELECT name, search::score(bio, 'lock compiler') AS relevance FROM people \
     WHERE bio MATCHES 'lock' ORDER BY relevance DESC;",
    "SELECT * FROM people ORDER BY vector::euclidean(at, [0.5, 0.0]) LIMIT 2;",
    "SELECT * FROM people ORDER BY vector::euclidean(at, [0.5, 0.0]) LIMIT 2 APPROXIMATE;",
    "SELECT * FROM people:1->follows->people;",
    "SELECT * FROM people WHERE name > 'b';",
    "GET sessions:'abc';",
    "GET sessions:'def';",
    "KEYS FROM sessions;",
    "SELECT count(*) AS held, address.city FROM people GROUP BY address.city;",
];

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

fn signed_in(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    // The last statement of the fixture declares a user, which closes the store,
    // so anything reading it afterwards has to say who it is.
    session.sign_in("root", "a long one").ok();
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

/// Every key and value of one keyspace.
fn dump(backend: &Arc<dyn KvBackend>, keyspace: Keyspace) -> Vec<(Vec<u8>, Vec<u8>)> {
    let request = ScanRequest {
        keyspace,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    backend
        .scan(&request)
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
        .collect()
}

/// A store with the fixture applied, and its backup.
fn original() -> (Arc<dyn KvBackend>, Store, Vec<u8>) {
    let (backend, store) = store();
    {
        let mut session = Session::new(&store);
        session.run(EVERYTHING).unwrap();
    }
    let mut taken = Vec::new();
    let written = bgv_db_backup::write(&store, &mut taken).unwrap();
    assert!(written.records > 0);
    assert_eq!(written.tail, store.committed_tail().unwrap());
    (backend, store, taken)
}

#[test]
fn a_restored_store_answers_exactly_what_the_original_did() {
    let (_, source, taken) = original();
    let (_, restored) = store();
    let outcome = bgv_db_backup::read(&restored, &mut taken.as_slice()).unwrap();
    assert!(!outcome.truncated);
    assert_eq!(outcome.tail, source.committed_tail().unwrap());
    assert_eq!(restored.committed_tail().unwrap(), outcome.tail);

    let mut here = signed_in(&source);
    let mut there = signed_in(&restored);
    for script in INTERROGATION {
        let expected = here.run(script).unwrap();
        let found = there.run(script).unwrap();
        assert_eq!(
            format!("{expected:?}"),
            format!("{found:?}"),
            "the two stores disagree about {script}"
        );
    }
}

#[test]
fn every_derived_byte_is_identical_because_it_was_derived() {
    // The claim, stated as bytes. Records, index entries, postings, the search
    // statistics, the vector graph, the catalog — none of them is in the backup,
    // and all of them come back. Anything that did not would be something this
    // store keeps outside its log, which is the defect this test exists to find.
    let (source, _held, taken) = original();
    let (target, restored) = store();
    bgv_db_backup::read(&restored, &mut taken.as_slice()).unwrap();

    for keyspace in Keyspace::ALL.iter().copied() {
        let expected = dump(&source, keyspace);
        let found = dump(&target, keyspace);
        assert_eq!(
            expected.len(),
            found.len(),
            "{keyspace:?} holds {} keys and the restore holds {}",
            expected.len(),
            found.len()
        );
        assert_eq!(expected, found, "{keyspace:?} differs after a restore");
    }
}

#[test]
fn a_restore_into_a_store_that_holds_something_is_refused() {
    // Merging a backup into a populated store is not a restore: the sequences
    // would collide with a different meaning and the result would be a store no
    // log explains.
    let (_, _held, taken) = original();
    let (target, occupied) = store();
    {
        let mut session = Session::new(&occupied);
        session.run("DEFINE NAMESPACE other;").unwrap();
    }
    let before = dump(&target, Keyspace::LOG);

    let refused = bgv_db_backup::read(&occupied, &mut taken.as_slice()).unwrap_err();
    assert!(matches!(refused, bgv_db_backup::Error::NotEmpty { .. }));
    // And nothing was written on the way to refusing.
    assert_eq!(dump(&target, Keyspace::LOG), before);
}

#[test]
fn a_file_that_is_not_one_is_refused_before_anything_is_applied() {
    let (target, restored) = store();
    for bytes in [
        b"not a backup at all".to_vec(),
        Vec::new(),
        b"BGVDBLO".to_vec(),
    ] {
        let refused = bgv_db_backup::read(&restored, &mut bytes.as_slice()).unwrap_err();
        assert!(
            matches!(refused, bgv_db_backup::Error::NotABackup),
            "{refused}"
        );
    }
    assert!(dump(&target, Keyspace::LOG).is_empty());
}

#[test]
fn a_version_this_build_does_not_read_is_refused_rather_than_guessed_at() {
    let (_, _held, taken) = original();
    for (position, what) in [(8_usize, "format"), (9, "record codec")] {
        let mut damaged = taken.clone();
        damaged[position] = 99;
        let (_, restored) = store();
        let refused = bgv_db_backup::read(&restored, &mut damaged.as_slice()).unwrap_err();
        match refused {
            bgv_db_backup::Error::Unsupported { what: named, .. } => assert_eq!(named, what),
            other => panic!("wrong refusal for {what}: {other}"),
        }
    }
}

#[test]
fn a_truncated_backup_restores_what_it_holds_and_says_so() {
    // A backup interrupted partway is still most of a store, and refusing it
    // would throw away the thing somebody is holding in a bad week.
    let (_, _held, taken) = original();
    let mut seen_partial = false;
    // From the end of the header onward: inside a length prefix, inside a
    // sequence, inside a record body, and cleanly between two records. A cut
    // *inside* the header is a different answer and has its own test.
    const HEADER: usize = 18;
    for cut in (HEADER..taken.len()).step_by(7) {
        let (_, restored) = store();
        let outcome = bgv_db_backup::read(&restored, &mut &taken[..cut]).unwrap();
        assert!(outcome.truncated, "a cut at {cut} was not noticed");
        assert!(
            outcome.records <= u64::try_from(taken.len()).unwrap(),
            "a cut at {cut} applied more than the file held"
        );
        if outcome.records > 0 {
            seen_partial = true;
        }
        // Whatever it applied, the store is consistent to that point.
        assert_eq!(restored.committed_tail().unwrap().get(), outcome.records);
    }
    assert!(seen_partial, "no cut left a partial restore to check");
}

#[test]
fn a_file_cut_inside_its_own_header_is_not_a_backup_at_all() {
    // The header says what the file is, so a file that has not finished saying
    // it cannot be read as one — and there is nothing to salvage before it.
    let (_, _held, taken) = original();
    for cut in 0..18 {
        let (_, restored) = store();
        let refused = bgv_db_backup::read(&restored, &mut &taken[..cut]).unwrap_err();
        assert!(
            matches!(refused, bgv_db_backup::Error::NotABackup),
            "a cut at {cut} gave {refused}"
        );
    }
}

#[test]
fn a_cut_exactly_between_two_records_is_still_noticed() {
    // The framing alone cannot see this: the file ends the way a whole one does.
    // The header's tail is what catches it, because sequences start at one and
    // cannot have gaps, so a backup taken at tail `n` holds exactly `n` records.
    let (_, _held, taken) = original();
    // The first record's frame is 12 bytes of header plus its body.
    let length = usize::try_from(u32::from_be_bytes([
        taken[18], taken[19], taken[20], taken[21],
    ]))
    .unwrap();
    let boundary = 18 + 12 + length;
    let (_, restored) = store();
    let outcome = bgv_db_backup::read(&restored, &mut &taken[..boundary]).unwrap();
    assert_eq!(outcome.records, 1);
    assert!(
        outcome.truncated,
        "a clean cut between records was not noticed"
    );
}

#[test]
fn a_whole_backup_is_not_reported_as_truncated() {
    let (_, _held, taken) = original();
    let (_, restored) = store();
    let outcome = bgv_db_backup::read(&restored, &mut taken.as_slice()).unwrap();
    assert!(!outcome.truncated);
}

#[test]
fn an_empty_store_backs_up_and_restores_to_an_empty_store() {
    // The degenerate case, which is also the one an automated backup hits first.
    let (_, source) = store();
    let mut taken = Vec::new();
    let written = bgv_db_backup::write(&source, &mut taken).unwrap();
    assert_eq!(written.records, 0);

    let (_, restored) = store();
    let outcome = bgv_db_backup::read(&restored, &mut taken.as_slice()).unwrap();
    assert_eq!(outcome.records, 0);
    assert!(!outcome.truncated);
    assert_eq!(restored.committed_tail().unwrap().get(), 0);
}

#[test]
fn a_restored_store_can_be_written_to_and_backed_up_again() {
    // A restore that produced a read-only museum piece would pass every test
    // above. The sequence has to continue from where the log left off.
    let (_, _held, taken) = original();
    let (_, restored) = store();
    let first = bgv_db_backup::read(&restored, &mut taken.as_slice()).unwrap();

    {
        let mut session = signed_in(&restored);
        session
            .run("CREATE people:9 = { name: 'after', email: 'e@x', at: [3.0, 0.0] };")
            .unwrap();
    }
    assert!(restored.committed_tail().unwrap().get() > first.records);

    let mut again = Vec::new();
    let written = bgv_db_backup::write(&restored, &mut again).unwrap();
    assert_eq!(written.records, first.records.saturating_add(1));
}
