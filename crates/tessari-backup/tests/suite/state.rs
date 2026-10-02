//! A snapshot of the store's current state, restored (ADR-0091).
//!
//! The claims, each held here by a test that fails when it is false:
//!
//! - a restored snapshot answers every read the source answers, on every engine;
//! - everything derived — index entries, postings, statistics, the vector graph,
//!   adjacency, counts — comes back byte for byte, because it was derived again;
//! - a snapshot is sized by what the store holds, not by how often it was written;
//! - a snapshot plus the log after it answers what the source answers now;
//! - a pruned store, which can take no log backup, takes a snapshot;
//! - a file that is not whole is refused, and nothing is written on the way;
//! - a writer running beside the snapshot does not tear it.

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{Keyspace, KvBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Sequence;

use crate::restore::{INTERROGATION, dump, one_log, original, signed_in, store};

/// Take a snapshot of `source` into bytes.
fn snapshot(source: &Store) -> Vec<u8> {
    let mut taken = Vec::new();
    tessari_backup::write_state(source, &mut taken).unwrap();
    taken
}

/// Restore `file` into a fresh store.
fn restored(file: &[u8]) -> (Arc<dyn KvBackend>, Store) {
    let (backend, target) = store();
    tessari_backup::read_state(&target, || Ok(file)).unwrap();
    (backend, target)
}

/// Every record as the store would answer it now, keyed without its version.
///
/// The newest stored version of each record, tombstones left out — computed
/// here from the raw keyspace rather than through the reader under test, so the
/// comparison does not share the code it is checking. `at` bounds the versions
/// considered, for a snapshot taken while writes continued.
fn current(backend: &Arc<dyn KvBackend>, at: Option<Sequence>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut held = BTreeMap::new();
    let mut decided: Option<Vec<u8>> = None;
    for (key, value) in dump(backend, Keyspace::DATA) {
        let (record, version) = key.split_at(key.len().saturating_sub(8));
        let version = !u64::from_be_bytes(version.try_into().unwrap());
        if decided.as_deref() == Some(record) || at.is_some_and(|at| version > at.get()) {
            continue;
        }
        decided = Some(record.to_vec());
        // A stamped value leads with its visibility byte; a tombstone is the one
        // that holds nothing, and the store does not answer it.
        let stamped =
            <tessari_encoding::StampedValue as tessari_encoding::StoreValue>::decode(&value)
                .unwrap();
        if matches!(stamped.value(), tessari_encoding::RecordValue::Tombstone) {
            continue;
        }
        held.insert(record.to_vec(), value);
    }
    held
}

#[test]
fn a_restored_snapshot_answers_exactly_what_the_original_did() {
    let (_, source, _) = original();
    let (_, target) = restored(&snapshot(&source));
    let mut here = signed_in(&source);
    let mut there = signed_in(&target);
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

/// Every derived byte of a restored snapshot is the source's, rebuilt.
///
/// Rebuilt, and not as the source holds it: two derived structures depend on the
/// history that produced them and a snapshot carries no history. A vector graph
/// keeps edges into records that were removed and a record that moved keeps the
/// place it was first inserted at, which is the decay `REBUILD INDEX` exists to
/// repair (`graph.rs`); a term's pruning bound is widened by a delete and never
/// narrowed again (ADR-0050). A restore derives both from the records as they
/// stand, which is what a rebuild does — so the source is rebuilt and the two are
/// then required to be the same bytes. The measured recall a rebuild writes is
/// left out: it is a measurement, which a restore has not taken. So are the
/// planner's statistics and change counters (`0x40`, `0x41`): facts about the
/// writes this node applied and the walks it took, which no copy carries.
#[test]
fn every_derived_byte_is_the_sources_rebuilt_because_it_was_derived_again() {
    let (source, held, _) = original();
    let (target, _restored) = restored(&snapshot(&held));
    assert_eq!(
        current(&source, None),
        current(&target, None),
        "the records the restored store answers differ from the source's"
    );
    let carried = tessari_backup::verify_state(&mut snapshot(&held).as_slice()).unwrap();
    let live = current(&source, None)
        .keys()
        .filter(|key| !counted(key))
        .count();
    assert_eq!(
        carried.records,
        u64::try_from(live).unwrap(),
        "the snapshot carries something besides the live records"
    );
    signed_in(&held)
        .run(
            "REBUILD INDEX by_email ON people; REBUILD INDEX by_city ON people; \
             REBUILD INDEX by_bio ON people; REBUILD INDEX by_at ON people;",
        )
        .unwrap();
    let measured = |held: &Vec<(Vec<u8>, Vec<u8>)>| {
        held.iter()
            .filter(|(key, _)| !matches!(key.first(), Some(&(0x17 | 0x40 | 0x41))))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        measured(&dump(&source, Keyspace::INDEX)),
        measured(&dump(&target, Keyspace::INDEX)),
        "an index, posting, statistic, graph node, edge or count differs after a restore"
    );
}

#[test]
fn a_snapshot_is_sized_by_what_the_store_holds_not_by_how_often_it_was_written() {
    let (_, source) = store();
    let mut session = Session::new(&source);
    session
        .run("DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; DEFINE COLLECTION t;")
        .unwrap();
    session.run("UPSERT t:1 = { n: 0 };").unwrap();
    let once = snapshot(&source);
    for n in 1..=1_000 {
        session.run(&format!("UPSERT t:1 = {{ n: {n} }};")).unwrap();
    }
    let often = snapshot(&source);
    let mut log = Vec::new();
    tessari_backup::write(&source, &mut log).unwrap();
    let before = tessari_backup::verify_state(&mut once.as_slice()).unwrap();
    let after = tessari_backup::verify_state(&mut often.as_slice()).unwrap();
    assert_eq!(
        before.records, after.records,
        "a thousand writes of one record changed how many records the snapshot holds"
    );
    assert!(
        log.len() > often.len().saturating_mul(50),
        "the snapshot ({} bytes) is not small beside the log ({} bytes)",
        often.len(),
        log.len()
    );
    let (_, target) = restored(&often);
    let mut there = Session::new(&target);
    let answer = there
        .run("USE NAMESPACE n; USE DATABASE d; GET t:1;")
        .unwrap();
    assert!(
        format!("{answer:?}").contains("1000"),
        "the last write is not what came back: {answer:?}"
    );
}

#[test]
fn a_snapshot_and_the_log_after_it_answer_what_the_source_answers_now() {
    let (source_backend, source, _) = one_log();
    let taken = snapshot(&source);
    let state = tessari_backup::verify_state(&mut taken.as_slice()).unwrap();
    let [(log, at)] = state.positions.as_slice() else {
        panic!(
            "the fixture holds one log, the snapshot names {:?}",
            state.positions
        );
    };
    {
        let mut session = Session::new(&source);
        session
            .run("DEFINE NAMESPACE five; DEFINE NAMESPACE six; USE NAMESPACE one; DEFINE DATABASE third;")
            .unwrap();
    }
    let mut after = Vec::new();
    tessari_backup::write_from(
        &source,
        &mut after,
        *log,
        Sequence::new(at.get().saturating_add(1)),
    )
    .unwrap();

    let (target_backend, target) = restored(&taken);
    tessari_backup::read(&target, &mut after.as_slice()).unwrap();
    assert_eq!(crate::tails(&target), crate::tails(&source));
    assert_eq!(
        current(&source_backend, None),
        current(&target_backend, None),
        "a snapshot plus the log after it does not hold what the source holds"
    );
}

#[test]
fn a_pruned_store_takes_a_snapshot_where_it_can_take_no_log_backup() {
    let (_, source, _) = original();
    for log in source.logs().unwrap() {
        let tail = source.committed_tail(log).unwrap();
        source.prune_log(log, tail).unwrap();
    }
    let mut refused = Vec::new();
    assert!(
        tessari_backup::write(&source, &mut refused).is_err(),
        "a pruned store produced a log backup that would restore without the pruned records"
    );
    let (_, target) = restored(&snapshot(&source));
    let mut here = signed_in(&source);
    let mut there = signed_in(&target);
    for script in INTERROGATION {
        assert_eq!(
            format!("{:?}", here.run(script).unwrap()),
            format!("{:?}", there.run(script).unwrap()),
            "a snapshot of a pruned store disagrees about {script}"
        );
    }
}

#[test]
fn a_snapshot_that_is_not_whole_is_refused_and_nothing_is_written() {
    let (_, source, _) = original();
    let taken = snapshot(&source);
    // Cut in the head, inside the first chunk, and one byte short of the end.
    for cut in [5, 60, taken.len().saturating_sub(1)] {
        let short = &taken[..cut];
        assert!(
            matches!(
                tessari_backup::verify_state(&mut &short[..]),
                Err(tessari_backup::Error::StateIncomplete | tessari_backup::Error::NotABackup)
            ),
            "a snapshot cut at byte {cut} was not refused"
        );
        let (target_backend, target) = store();
        assert!(tessari_backup::read_state(&target, || Ok(short)).is_err());
        assert!(
            target.holds_nothing().unwrap(),
            "a refused snapshot wrote something"
        );
        assert!(dump(&target_backend, Keyspace::DATA).is_empty());
    }
    // A first chunk claiming four gigabytes: refused as not whole, having read
    // no further than the file goes. Its length follows the head, the per-log
    // positions and the chunk's tag.
    let logs = tessari_backup::verify_state(&mut taken.as_slice())
        .unwrap()
        .positions
        .len();
    let first = logs
        .saturating_mul(1 + 25 + 8)
        .saturating_add(11 + 2 + 12 + 8 + 4 + 1);
    let mut huge = taken.clone();
    huge[first..first.saturating_add(4)].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        tessari_backup::verify_state(&mut huge.as_slice()),
        Err(tessari_backup::Error::StateIncomplete)
    ));
    // One flipped byte inside the records.
    let mut damaged = taken.clone();
    let inside = damaged.len() / 2;
    damaged[inside] ^= 0x40;
    assert!(
        tessari_backup::verify_state(&mut damaged.as_slice()).is_err(),
        "a damaged snapshot verified"
    );
}

#[test]
fn a_snapshot_is_refused_by_a_store_that_holds_something() {
    let (_, source, _) = original();
    let (_, occupied) = store();
    Session::new(&occupied)
        .run("DEFINE NAMESPACE other;")
        .unwrap();
    let taken = snapshot(&source);
    let refused = tessari_backup::read_state(&occupied, || Ok(taken.as_slice()));
    assert!(matches!(
        refused,
        Err(tessari_backup::Error::WrongBase { .. })
    ));
}

#[test]
fn a_snapshot_from_a_newer_build_is_refused() {
    let (_, source, _) = original();
    let mut taken = snapshot(&source);
    // The writer's major version follows the name and the two format bytes.
    let major = 11 + 2;
    taken[major..major + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        tessari_backup::verify_state(&mut taken.as_slice()),
        Err(tessari_backup::Error::WrittenByNewer { .. })
    ));
}

/// Writes committed after a snapshot began are not in it.
///
/// Deterministic rather than raced: the reader is opened, the writes are
/// committed, and only then is the state read out — so every record the writes
/// touched has a version the reader must not see. A thread racing the snapshot
/// proved nothing here, because the writer could finish before the snapshot
/// began or begin after it ended, and either way the test passed.
#[test]
fn writes_committed_after_a_snapshot_began_are_not_in_it() {
    let (source_backend, source) = store();
    let mut session = Session::new(&source);
    session
        .run("DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; DEFINE COLLECTION t;")
        .unwrap();
    for n in 0..20 {
        session
            .run(&format!("CREATE t:{n} = {{ n: {n} }};"))
            .unwrap();
    }
    let mut reader = source.read_state().unwrap();
    let at = reader.version();
    for n in 0_u32..20 {
        session
            .run(&format!(
                "UPSERT t:{n} = {{ n: {}, later: true }};",
                n.saturating_add(100)
            ))
            .unwrap();
    }
    session.run("CREATE t:99 = { n: 99 }; DELETE t:0;").unwrap();
    let mut read = BTreeMap::new();
    while let Some(chunk) = reader.next_chunk(7).unwrap() {
        for mutation in chunk.mutations() {
            let mut key = tessari_encoding::RecordKey::versions_prefix(
                mutation.namespace,
                mutation.database,
                mutation.table,
                &mutation.id,
            );
            key.shrink_to_fit();
            read.insert(
                key,
                <tessari_encoding::StampedValue as tessari_encoding::StoreValue>::encode(
                    &mutation.value,
                )
                .as_slice()
                .to_vec(),
            );
        }
    }
    let expected: BTreeMap<_, _> = current(&source_backend, Some(at))
        .into_iter()
        .filter(|(key, _)| !counted(key))
        .collect();
    assert_eq!(
        read, expected,
        "the state read is not the store as it stood when the read began"
    );
}

/// Whether a record key is one of the per-table counts a restore derives again.
fn counted(key: &[u8]) -> bool {
    key.starts_with(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 18])
}

/// A topic emptied by retention carries on numbering after a restore.
///
/// Its last-given position is the one thing a topic holds that no surviving
/// message can say: once retention has removed every message, the records
/// restored hold nothing to count from, and a topic rebuilt from them alone
/// would hand out position 1 again — to a reader that has already acted on the
/// first message that had it.
#[test]
fn a_topic_emptied_by_retention_carries_on_numbering_after_a_restore() {
    let (_, source) = store();
    let mut session = Session::new(&source);
    session
        .run("DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; DEFINE TOPIC short RETAIN 50ms;")
        .unwrap();
    for n in 1..=4 {
        session
            .run(&format!("CREATE short:'m{n}' = {{ n: {n} }};"))
            .unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert_eq!(
        source.remove_expired().unwrap().records,
        4,
        "retention did not empty the topic"
    );
    let (_, target) = restored(&snapshot(&source));
    let mut there = Session::new(&target);
    there
        .run("USE NAMESPACE n; USE DATABASE d; CREATE short:'m5' = { n: 5 };")
        .unwrap();
    let answer = there.run("READ FROM short;").unwrap();
    let positions: Vec<u64> = answer[0]
        .records()
        .unwrap()
        .iter()
        .map(|(_, body)| {
            let tessari_types::Value::Object(fields) = body else {
                panic!("a message answered {body:?}");
            };
            let Some(tessari_types::Value::Number(tessari_types::Number::Integer(position))) =
                fields.get("position")
            else {
                panic!("no position in {fields:?}");
            };
            u64::try_from(*position).unwrap()
        })
        .collect();
    assert_eq!(
        positions,
        vec![5],
        "the restored topic started numbering again"
    );
}
