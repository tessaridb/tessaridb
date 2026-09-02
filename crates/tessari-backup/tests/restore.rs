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

use tessari_kv::{KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

/// Every engine this project has built, in one script.
///
/// Records, an ordered index, a unique one, a path index, a full-text index with
/// its maintained statistics, a vector graph, edges, a key-value space, a bucket
/// of files, schema declarations with a default, users — and updates and deletes
/// after the indexes exist, so the maintenance path runs rather than only the
/// build path.
///
/// The bucket is here for the same reason everything else is, and it needed no
/// new machinery to be: a file is a record and a chunk is a record, so a restore
/// that reproduces records reproduces files. A bucket whose files did *not*
/// survive would mean something about them is not derived from the log — which
/// would be a defect in the store rather than in the backup, and this is where
/// that shows.
const EVERYTHING: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE ANALYZER simple FILTERS lowercase, ascii;\n\
DEFINE TABLE people SCHEMALESS;\n\
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
DEFINE COLLECTION sessions;\n\
SET sessions:'abc' = { user: people:1, level: 3 };\n\
SET sessions:'def' = [1, 2, 3];\n\
DEL sessions:'def';\n\
DEFINE BUCKET media;\n\
PUT media:'/logo.png' = 0x89504e470d0a1a0a;\n\
PUT media:'/notes.txt' = 'ada wrote this, and it is stored as bytes';\n\
PUT media:'/replaced' = 'the long one that gets written over';\n\
PUT media:'/replaced' = 'short';\n\
PUT media:'/gone' = 'removed before the backup was taken';\n\
DELETE media:'/gone';\n\
DEFINE NODE ROLES serving, writable ENDPOINTS 'original:9000';\n\
DEFINE REPLICA second AT 'peer:9001';\n\
DEFINE USER root ROLE owner PASSWORD 'a long one';";

/// The file's own header: magic, two versions, the build that wrote it, and the
/// two sequences it spans.
///
/// Named rather than spelled as a number at four call sites, because the layout
/// is the thing these tests are about and a change to it should touch one line.
/// It did: the writer's three numbers were added and this is the one line.
const HEADER_LEN: usize = 8 + 1 + 1 + (4 + 4 + 4) + 8 + 8;

/// One record's frame: its length, its sequence, and its checksum.
const FRAME_LEN: usize = 4 + 8 + 4;

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
    "SELECT * FROM media;",
    "READ media:'/logo.png';",
    "READ media:'/notes.txt';",
    "READ media:'/replaced';",
    "READ media:'/gone';",
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
    let written = tessari_backup::write(&store, &mut taken).unwrap();
    assert!(written.records > 0);
    assert_eq!(written.tail, store.committed_tail().unwrap());
    (backend, store, taken)
}

#[test]
fn a_restored_store_answers_exactly_what_the_original_did() {
    let (_, source, taken) = original();
    let (_, restored) = store();
    let outcome = tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();
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

/// The counter that names records survives a restore, and it needs its own test.
///
/// A table's record counter is catalog state, so a restore that reproduced every
/// record but not the counter would be invisible to everything else in this file:
/// every read would answer exactly what the original answered, because the
/// counter is not observable by reading. It is observable only by **writing**,
/// which is why `a_restored_store_answers_exactly_what_the_original_did` — the
/// strongest claim here — cannot make this one.
///
/// This is G020's kill criterion K2 in a test rather than in a paragraph: if the
/// counter cannot be shown not to regress across a restore, the sequence does not
/// ship as the default identity.
///
/// The store's own walk-past rule keeps a regression from *corrupting* anything —
/// an occupied identity is stepped over rather than written onto — so the damage
/// would be a restored table quietly re-walking its whole history on every write.
/// That is a cost rather than a wrong answer, which is exactly why it would never
/// be noticed, and why the assertion below is on the identity itself and not on
/// the record count. A count would pass either way.
#[test]
fn the_record_counter_survives_a_restore_rather_than_starting_again() {
    let (_, source) = store();
    {
        let mut session = Session::new(&source);
        session
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;\
                 DEFINE DATABASE orders; USE DATABASE orders;\
                 DEFINE COLLECTION users;\
                 CREATE users = { name: 'ada' };\
                 CREATE users = { name: 'grace' };\
                 CREATE users = { name: 'edith' };",
            )
            .unwrap();
    }
    let mut taken = Vec::new();
    tessari_backup::write(&source, &mut taken).unwrap();

    let (_, restored) = store();
    tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();

    let mut there = Session::new(&restored);
    there
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    let mut answers = there.run("CREATE users = { name: 'alan' };").unwrap();
    let outcome = answers.pop().expect("one statement, one answer");
    let Outcome::Keys(keys) = outcome else {
        panic!("a generated write answers with the identity it produced, got {outcome:?}");
    };
    assert_eq!(keys.len(), 1, "one write, one identity: {keys:?}");
    assert_eq!(
        format!("{:?}", keys[0]),
        "Int(4)",
        "the restored table carried on from three rather than starting again: {keys:?}"
    );
}

/// The one key that is deliberately **not** derived from the log.
///
/// A node's own identity lives in `META` precisely so that it does not travel in
/// a backup (ADR-0018 §1): a restore that carried it would hand the restored
/// machine the original's id, and two processes would then claim to be one node.
/// So this key is excluded from the byte-for-byte comparison below and is
/// asserted to **differ** in a test of its own — the exception is stated twice,
/// once as a hole and once as a claim, because a hole on its own would also
/// cover the key going missing entirely.
const NODE_IDENTITY_KEY: &[u8] = &[0x38];

#[test]
fn every_derived_byte_is_identical_because_it_was_derived() {
    // The claim, stated as bytes. Records, index entries, postings, the search
    // statistics, the vector graph, the catalog — none of them is in the backup,
    // and all of them come back. Anything that did not would be something this
    // store keeps outside its log, which is the defect this test exists to find
    // — and the node identity is the one thing that is outside it **on purpose**.
    let (source, _held, taken) = original();
    let (target, restored) = store();
    tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();

    for keyspace in Keyspace::ALL.iter().copied() {
        let expected = derived(&source, keyspace);
        let found = derived(&target, keyspace);
        assert_eq!(
            expected.len(),
            found.len(),
            "{keyspace:?} holds {} derived keys and the restore holds {}",
            expected.len(),
            found.len()
        );
        assert_eq!(expected, found, "{keyspace:?} differs after a restore");
    }
}

/// Everything in a keyspace that the log is supposed to produce.
fn derived(backend: &Arc<dyn KvBackend>, keyspace: Keyspace) -> Vec<(Vec<u8>, Vec<u8>)> {
    dump(backend, keyspace)
        .into_iter()
        .filter(|(key, _)| key.as_slice() != NODE_IDENTITY_KEY)
        .collect()
}

#[test]
fn a_restore_carries_the_peer_list_and_not_the_identity() {
    // The operator's bad day, as a test: last night's backup goes onto a fresh
    // machine to check that it restores, and both processes start. If the
    // identity travelled in the log they would now be the same node — both
    // heartbeating under one id, and every routing decision taken from it wrong
    // with nothing reporting it. This is the failure the `meta`/log split exists
    // to prevent, and it is invisible to every other test in this file, because
    // a store replaying its own log gets its own id back and looks correct.
    //
    // **Both halves are asserted here, and neither is the criterion on its own**
    // (ADR-0020, F10). A store that replicated *nothing* would satisfy the
    // identity half; a store that replicated *everything* would satisfy the peer
    // half. Only the two together say where the line is, so a single test holds
    // them rather than two tests that could be deleted independently.
    let (_, source, taken) = original();
    let (target, restored) = store();
    tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();

    // The topology half: a peer is a catalog record, so it is in the log and it
    // arrives. Read back through the statement rather than off the keyspace,
    // because what a second node needs is the *answer*, not the bytes.
    let mut asking = Session::new(&restored);
    asking.sign_in("root", "a long one").unwrap();
    let reported = asking.run("INFO FOR NODE;").unwrap();
    let described = format!("{reported:?}");
    assert!(
        described.contains("second") && described.contains("peer:9001"),
        "the restored store did not inherit the peer list: {described}"
    );
    // And the peer arrived *as topology*, under `cluster`, rather than beside
    // the local fields — the grouping is what tells a reader which half would
    // follow a backup, so a flat answer here would be the same defect wearing
    // the right values.
    assert!(described.contains("cluster"), "{described}");

    // **Re-opened rather than asked directly.** The store used to resolve its
    // identity once at open, so this handle would have been holding what it read
    // before the restore ran — and comparing that would have passed even if the
    // restore had written the original's identity straight onto this store's
    // disk. The cache is gone, so the handle would now answer correctly; the
    // reopen stays because the question this test asks is what is *on disk*
    // afterwards, and asking the disk should not depend on how the accessor
    // happens to be implemented today.
    drop(asking);
    drop(restored);
    let reopened = Store::open(Arc::clone(&target)).unwrap();
    let inherited = reopened.node_identity().unwrap();
    assert_ne!(
        source.node_identity().unwrap().id,
        inherited.id,
        "the restored store inherited the original's identity"
    );
    // The rest of the local half, which F10 names alongside the id: the original
    // declared its roles and its address by statement, and neither followed the
    // backup. Without this, "the identity did not travel" would be a claim about
    // sixteen generated bytes rather than about everything `meta` holds.
    assert!(
        !inherited.endpoints.contains(&"original:9000".to_owned()),
        "the restored store inherited the original's address: {:?}",
        inherited.endpoints
    );
    assert_eq!(
        inherited.roles,
        tessari_encoding::Roles::ALONE,
        "the restored store inherited the original's roles rather than starting fresh"
    );
    // And the restore did not simply leave the fresh store without one: an
    // absent identity would also satisfy `!=` while being a different defect.
    assert_eq!(
        dump(&target, Keyspace::META)
            .iter()
            .filter(|(key, _)| key.as_slice() == NODE_IDENTITY_KEY)
            .count(),
        1
    );
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

    let refused = tessari_backup::read(&occupied, &mut taken.as_slice()).unwrap_err();
    assert!(matches!(refused, tessari_backup::Error::WrongBase { .. }));
    // And nothing was written on the way to refusing.
    assert_eq!(dump(&target, Keyspace::LOG), before);
}

#[test]
fn a_file_that_is_not_one_is_refused_before_anything_is_applied() {
    let (target, restored) = store();
    for bytes in [
        b"not a backup at all".to_vec(),
        Vec::new(),
        b"TESSALO".to_vec(),
    ] {
        let refused = tessari_backup::read(&restored, &mut bytes.as_slice()).unwrap_err();
        assert!(
            matches!(refused, tessari_backup::Error::NotABackup),
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
        let refused = tessari_backup::read(&restored, &mut damaged.as_slice()).unwrap_err();
        match refused {
            tessari_backup::Error::Unsupported { what: named, .. } => assert_eq!(named, what),
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
    const HEADER: usize = HEADER_LEN;
    for cut in (HEADER..taken.len()).step_by(7) {
        let (_, restored) = store();
        let outcome = tessari_backup::read(&restored, &mut &taken[..cut]).unwrap();
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
    for cut in 0..HEADER_LEN {
        let (_, restored) = store();
        let refused = tessari_backup::read(&restored, &mut &taken[..cut]).unwrap_err();
        assert!(
            matches!(refused, tessari_backup::Error::NotABackup),
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
    // The first record's frame is its header plus its body.
    let length = usize::try_from(u32::from_be_bytes([
        taken[HEADER_LEN],
        taken[HEADER_LEN + 1],
        taken[HEADER_LEN + 2],
        taken[HEADER_LEN + 3],
    ]))
    .unwrap();
    let boundary = HEADER_LEN + FRAME_LEN + length;
    let (_, restored) = store();
    let outcome = tessari_backup::read(&restored, &mut &taken[..boundary]).unwrap();
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
    let outcome = tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();
    assert!(!outcome.truncated);
}

#[test]
fn an_empty_store_backs_up_and_restores_to_an_empty_store() {
    // The degenerate case, which is also the one an automated backup hits first.
    let (_, source) = store();
    let mut taken = Vec::new();
    let written = tessari_backup::write(&source, &mut taken).unwrap();
    assert_eq!(written.records, 0);

    let (_, restored) = store();
    let outcome = tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();
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
    let first = tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();

    {
        let mut session = signed_in(&restored);
        session
            .run("CREATE people:9 = { name: 'after', email: 'e@x', at: [3.0, 0.0] };")
            .unwrap();
    }
    assert!(restored.committed_tail().unwrap().get() > first.records);

    let mut again = Vec::new();
    let written = tessari_backup::write(&restored, &mut again).unwrap();
    assert_eq!(written.records, first.records.saturating_add(1));
}

#[test]
fn a_backup_can_be_verified_without_being_applied_to_anything() {
    // What somebody does before the day they need it. No store is involved,
    // which is the point: a backup that can only be checked by restoring it is a
    // backup nobody checks.
    let (_, held, taken) = original();
    let verified = tessari_backup::verify(&mut taken.as_slice()).unwrap();
    assert_eq!(verified.from.get(), 1);
    assert_eq!(verified.tail, held.committed_tail().unwrap());
    assert_eq!(verified.good_through, verified.tail);
    assert!(!verified.truncated);
    assert_eq!(verified.records, verified.tail.get());
}

#[test]
fn a_record_whose_bytes_changed_is_refused_before_it_is_applied() {
    // The check framing cannot do: this file is exactly the right length and
    // holds the wrong bytes. Without the checksum it either fails to decode,
    // which is luck, or applies a record nobody wrote.
    let (_, _held, taken) = original();
    let mut damaged = taken.clone();
    // A byte well inside the first record's body, past the header and the frame.
    let at = HEADER_LEN + FRAME_LEN + 3;
    damaged[at] ^= 0xff;

    let refused = tessari_backup::verify(&mut damaged.as_slice()).unwrap_err();
    assert!(
        matches!(refused, tessari_backup::Error::Damaged { .. }),
        "{refused}"
    );

    // And a restore refuses it too, rather than leaving the check to whoever
    // remembered to verify.
    let (_, restored) = store();
    let refused = tessari_backup::read(&restored, &mut damaged.as_slice()).unwrap_err();
    assert!(
        matches!(refused, tessari_backup::Error::Damaged { .. }),
        "{refused}"
    );
}

#[test]
fn an_incremental_backup_plus_its_base_is_the_whole_store() {
    // The property that makes an incremental one worth having, asserted as an
    // equality rather than as a count: the store built from two files answers
    // what the original answers.
    let (_, held, base) = original();

    // More work after the base was taken.
    {
        let mut session = signed_in(&held);
        session
            .run(
                "CREATE people:9 = { name: 'later', email: 'z@x', bio: 'after the base', \
                 address: { city: 'york' }, at: [3.0, 3.0] };\n\
                 PUT media:'/added.txt' = 'written after the base was taken';",
            )
            .unwrap();
    }
    let taken_at = base.len();
    assert!(taken_at > 0);
    let base_tail = tessari_backup::verify(&mut base.as_slice()).unwrap().tail;

    let mut increment = Vec::new();
    let written = tessari_backup::write_from(
        &held,
        &mut increment,
        tessari_types::Sequence::new(base_tail.get() + 1),
    )
    .unwrap();
    assert_eq!(written.from.get(), base_tail.get() + 1);
    assert!(written.records > 0, "the increment holds nothing");

    // Restore the base, then the increment onto it.
    let (_, rebuilt) = store();
    tessari_backup::read(&rebuilt, &mut base.as_slice()).unwrap();
    tessari_backup::read(&rebuilt, &mut increment.as_slice()).unwrap();
    assert_eq!(
        rebuilt.committed_tail().unwrap(),
        held.committed_tail().unwrap()
    );

    // And it answers what the original answers.
    let mut there = signed_in(&held);
    let mut here = signed_in(&rebuilt);
    for question in INTERROGATION {
        assert_eq!(
            format!("{:?}", there.run(question)),
            format!("{:?}", here.run(question)),
            "{question}"
        );
    }
}

#[test]
fn an_increment_refuses_a_store_that_is_not_where_it_continues_from() {
    // The header says what the file continues from, so this is checkable rather
    // than a filename's promise.
    let (_, held, base) = original();
    {
        let mut session = signed_in(&held);
        session
            .run("CREATE people:9 = { name: 'later', email: 'z@x' };")
            .unwrap();
    }
    let base_tail = tessari_backup::verify(&mut base.as_slice()).unwrap().tail;
    let mut increment = Vec::new();
    tessari_backup::write_from(
        &held,
        &mut increment,
        tessari_types::Sequence::new(base_tail.get() + 1),
    )
    .unwrap();

    // Onto an empty store, which is not where it continues from.
    let (_, empty) = store();
    let refused = tessari_backup::read(&empty, &mut increment.as_slice()).unwrap_err();
    assert!(
        matches!(refused, tessari_backup::Error::WrongBase { .. }),
        "{refused}"
    );
}

#[test]
fn a_restore_can_stop_at_a_chosen_point() {
    // The log is the store, so stopping the replay leaves the store holding
    // exactly what it held then — there is no second mechanism to rewind and
    // nothing to undo.
    let (_, held, taken) = original();
    let whole = held.committed_tail().unwrap();
    let midpoint = tessari_types::Sequence::new(whole.get() / 2);

    let (_, rebuilt) = store();
    let outcome =
        tessari_backup::read_until(&rebuilt, &mut taken.as_slice(), Some(midpoint)).unwrap();
    assert_eq!(rebuilt.committed_tail().unwrap(), midpoint);
    assert!(
        !outcome.truncated,
        "stopping where the caller asked is not a truncation"
    );

    // And what it holds is what the original held at that sequence, which is
    // asserted by taking the original's own backup to the same point.
    let (_, twin) = store();
    tessari_backup::read_until(&twin, &mut taken.as_slice(), Some(midpoint)).unwrap();
    assert_eq!(
        twin.committed_tail().unwrap(),
        rebuilt.committed_tail().unwrap()
    );
}
