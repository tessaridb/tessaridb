//! The durability claim, proven by killing the process that made it.
//!
//! The claim is one sentence: **a transaction whose commit returned at
//! `PowerLossSafe` is present after the writing process is killed without a
//! clean close.** Everything here exists to test that sentence rather than to
//! restate it.
//!
//! The shape is a child process that commits and announces each sequence it was
//! given, and a parent that reads a fixed number of those announcements and then
//! sends the child an uncatchable kill. Nothing runs on the way down: no
//! destructor, no flush, no close. The parent then reopens the store and
//! compares what is present against what was announced.
//!
//! It also checks the cross-region invariant: a commit writes the record into
//! one region and the position that accounts for it into another, in one batch,
//! and the reopened store must not show a committed position with no record
//! behind it.
//!
//! # What these tests do not prove
//!
//! Being explicit about this is the point, because each gap is easy to read past
//! and none of them is closed by the tests passing.
//!
//! **It does not discriminate the two durability levels.** A kill signal does
//! not clear the operating system's page cache, so a store running without
//! per-commit sync would pass this too. What the test establishes is that the
//! commit path, the recovery path and the cross-region invariant are sound; the
//! power-loss half of the claim rests on the sync itself, and on a platform
//! whose `fsync` reaches the device rather than the drive's own cache. That
//! belongs on hardware, in the readiness checklist, and not here.
//!
//! **The first test does not exercise a flush at all.** At twenty small records
//! nothing has left memory, so recovery is pure log replay, where a batch is one
//! record and cross-region atomicity is trivially preserved. The second test
//! exists for that: it writes past a deliberately lowered memtable ceiling, so
//! sorted files are on disk before the kill — asserted, not assumed — and
//! recovery is mixed, part read from files and part replayed, with the same two
//! invariants holding across the seam.
//!
//! **Neither test reaches a genuinely divergent flush, and that is a property of
//! the store rather than a gap.** Two independent reasons: the store never calls
//! flush, so every flush is auto-triggered, and an auto-triggered flush under
//! `atomic_flush` covers every region at once; and the WAL is enabled on every
//! write at both durability levels, which is the condition under which the
//! engine's own header says atomic flush is unnecessary for recovery in the
//! first place. See the correctness list in `crate::options`.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use bgv_db_kv::KvBackend;
use bgv_db_lsm::{Durability, LsmBackend, StoreConfig};
use bgv_db_storage::{RecordAddress, Store};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

/// Environment variable carrying the store path into the child.
const STORE_PATH: &str = "BGV_DB_DURABILITY_STORE";
/// How many acknowledged commits the parent waits for before killing the child.
const ACKNOWLEDGED_BEFORE_KILL: usize = 20;
/// Where the child gives up on its own.
///
/// It exists only so the child cannot outlive a parent that died before killing
/// it. The parent ends it three orders of magnitude before this.
const CHILD_GIVES_UP_AFTER: u64 = 100_000;

fn address(n: u64) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(format!("record-{n:04}")),
    )
}

fn payload(n: u64) -> Vec<u8> {
    format!("payload-{n}").into_bytes()
}

fn open_store(path: &std::path::Path) -> Store {
    let backend = LsmBackend::open(path, StoreConfig::new(Durability::PowerLossSafe)).unwrap();
    Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
}

/// The child. Commits forever, announcing each sequence it was handed.
///
/// It is `#[ignore]`d because it never returns on its own — the parent ends it.
#[test]
#[ignore = "spawned by the durability test; runs until it is killed"]
fn commit_until_killed() {
    let Ok(path) = std::env::var(STORE_PATH) else {
        panic!("{STORE_PATH} must name the store directory");
    };
    let store = open_store(std::path::Path::new(&path));

    for n in 1..=CHILD_GIVES_UP_AFTER {
        let mut transaction = store.begin().unwrap();
        transaction.put(address(n), payload(n));
        let committed = transaction.commit().unwrap();
        // Printed only after the commit returned, so every line the parent reads
        // is a promise the store made.
        println!("committed {n} {committed}");
        // The harness buffers stdout, and an announcement the parent never reads
        // proves nothing.
        use std::io::Write;
        std::io::stdout().flush().unwrap();
    }
}

#[test]
fn an_acknowledged_commit_survives_the_writer_being_killed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "commit_until_killed", "--ignored", "--nocapture"])
        .env(STORE_PATH, &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut acknowledged: Vec<(u64, Sequence)> = Vec::new();
    {
        let stdout = child.stdout.take().unwrap();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            let mut parts = line.split_whitespace();
            if parts.next() != Some("committed") {
                continue;
            }
            let (Some(n), Some(sequence)) = (parts.next(), parts.next()) else {
                continue;
            };
            acknowledged.push((n.parse().unwrap(), Sequence::new(sequence.parse().unwrap())));
            if acknowledged.len() >= ACKNOWLEDGED_BEFORE_KILL {
                break;
            }
        }
    }

    // No unwinding, no destructor, no flush — the same thing a power cut does to
    // the process, minus the page cache.
    child.kill().unwrap();
    let _ = child.wait();

    assert_eq!(
        acknowledged.len(),
        ACKNOWLEDGED_BEFORE_KILL,
        "the child died before it acknowledged enough commits to test anything"
    );

    let reopened = open_store(&path);
    let transaction = reopened.begin().unwrap();
    for (n, sequence) in &acknowledged {
        assert_eq!(
            transaction.get(&address(*n)).unwrap(),
            Some(payload(*n)),
            "record {n}, acknowledged at sequence {sequence}, did not survive the kill"
        );
    }

    // The record lives in one region and the position that accounts for it in
    // another. A committed position behind the last acknowledged sequence means
    // the two regions were recovered to different points.
    let (_, last) = acknowledged.last().unwrap();
    assert!(
        reopened.committed_tail().unwrap() >= *last,
        "the committed position was recovered behind the records it accounts for"
    );
}

/// Environment variable carrying the store path into the flushing child.
const FLUSHED_STORE_PATH: &str = "BGV_DB_FLUSHED_STORE";
/// A memtable ceiling low enough that a few hundred commits cross it, and high
/// enough to stay above the engine's own arena floor.
///
/// The floor is not obvious and it is what makes a smaller value useless: each
/// region's memtable allocates in arena blocks of up to 1 MiB
/// (`arena_block_size`, auto-computed as the smaller of 1 MiB and an eighth of
/// the per-region write buffer), and this store has five regions. A ceiling
/// below that is exceeded by the allocator before it holds any data, and the
/// engine flushes every region continuously — which tests the wrong thing very
/// thoroughly.
const SMALL_MEMTABLE_BYTES: usize = 16 * 1024 * 1024;
/// Payload size, chosen against the ceiling above so that a few hundred commits
/// cross it more than once.
const LARGE_PAYLOAD_BYTES: usize = 64 * 1024;
/// How many acknowledged commits the parent waits for before killing. At the
/// sizes above this is roughly a hundred megabytes, so files exist on disk well
/// before the kill.
const ACKNOWLEDGED_BEFORE_FLUSHED_KILL: usize = 400;

fn large_payload(n: u64) -> Vec<u8> {
    let mut payload = format!("payload-{n}-").into_bytes();
    payload.resize(LARGE_PAYLOAD_BYTES, b'.');
    payload
}

fn open_small_store(path: &std::path::Path) -> Store {
    let config = StoreConfig {
        memtable_bytes: SMALL_MEMTABLE_BYTES,
        ..StoreConfig::new(Durability::PowerLossSafe)
    };
    let backend = LsmBackend::open(path, config).unwrap();
    Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
}

/// Whether the store has written any sorted file, which is how a flush shows up.
///
/// Read from the directory rather than from an engine counter, because the
/// question is whether anything reached disk, and a file on disk is the answer
/// in its least deniable form.
fn sorted_files(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sst"))
        .count()
}

/// The flushing child. Same shape as [`commit_until_killed`], larger records and
/// a small memtable ceiling, so that flushing is under way when it is killed.
#[test]
#[ignore = "spawned by the flushed durability test; runs until it is killed"]
fn commit_large_until_killed() {
    let Ok(path) = std::env::var(FLUSHED_STORE_PATH) else {
        panic!("{FLUSHED_STORE_PATH} must name the store directory");
    };
    let store = open_small_store(std::path::Path::new(&path));

    for n in 1..=CHILD_GIVES_UP_AFTER {
        let mut transaction = store.begin().unwrap();
        transaction.put(address(n), large_payload(n));
        let committed = transaction.commit().unwrap();
        println!("committed {n} {committed}");
        use std::io::Write;
        std::io::stdout().flush().unwrap();
    }
}

#[test]
fn an_acknowledged_commit_survives_a_kill_after_the_store_has_flushed() {
    // The gap the test above names: at twenty small records nothing has left
    // memory, so recovery is pure log replay and a batch is one record. Here
    // files exist before the kill, so recovery is mixed — part read from disk,
    // part replayed — and the same two invariants have to hold across the seam.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "commit_large_until_killed",
            "--ignored",
            "--nocapture",
        ])
        .env(FLUSHED_STORE_PATH, &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut acknowledged: Vec<(u64, Sequence)> = Vec::new();
    {
        let stdout = child.stdout.take().unwrap();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            let mut parts = line.split_whitespace();
            if parts.next() != Some("committed") {
                continue;
            }
            let (Some(n), Some(sequence)) = (parts.next(), parts.next()) else {
                continue;
            };
            acknowledged.push((n.parse().unwrap(), Sequence::new(sequence.parse().unwrap())));
            if acknowledged.len() >= ACKNOWLEDGED_BEFORE_FLUSHED_KILL {
                break;
            }
        }
    }

    child.kill().unwrap();
    let _ = child.wait();

    assert_eq!(
        acknowledged.len(),
        ACKNOWLEDGED_BEFORE_FLUSHED_KILL,
        "the child died before it acknowledged enough commits to test anything"
    );
    // Without this the test would still pass while proving only what the other
    // one proves.
    assert!(
        sorted_files(&path) > 0,
        "nothing was flushed, so this is the log-replay case again: {}",
        sorted_files(&path)
    );

    let reopened = open_small_store(&path);
    let transaction = reopened.begin().unwrap();
    for (n, sequence) in &acknowledged {
        assert_eq!(
            transaction.get(&address(*n)).unwrap(),
            Some(large_payload(*n)),
            "record {n}, acknowledged at sequence {sequence}, did not survive the kill"
        );
    }

    let (_, last) = acknowledged.last().unwrap();
    assert!(
        reopened.committed_tail().unwrap() >= *last,
        "the committed position was recovered behind the records it accounts for"
    );
}
