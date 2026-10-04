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
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use tessari_kv::KvBackend;
use tessari_lsm::{Durability, LsmBackend, StoreConfig};
use tessari_storage::{RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

/// Environment variable carrying the store path into the child.
const STORE_PATH: &str = "TESSARIDB_DURABILITY_STORE";
/// How many acknowledged commits the parent waits for before killing the child.
const ACKNOWLEDGED_BEFORE_KILL: usize = 20;
/// Where the child gives up on its own.
///
/// It exists only so the child cannot outlive a parent that died before killing
/// it. The parent ends it three orders of magnitude before this.
const CHILD_GIVES_UP_AFTER: u64 = 100_000;

/// The log every record this test writes lands in.
///
/// `address` names namespace 1 and database 1, so the position `commit` answers
/// with counts in that database's own log and in no other (S6.2). Reading the
/// store's log here would compare a promise made about one counter against
/// another that never moved.
const HOME: tessari_types::Reach =
    tessari_types::Reach::Database(NamespaceId::new(1), DatabaseId::new(1));

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
        reopened
            .committed_tail(reopened.own_log(HOME).unwrap())
            .unwrap()
            >= *last,
        "the committed position was recovered behind the records it accounts for"
    );
}

/// Environment variable carrying the store path into the flushing child.
const FLUSHED_STORE_PATH: &str = "TESSARIDB_FLUSHED_STORE";
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

/// Set in the flushing child's environment when its store is encrypted.
const FLUSHED_ENCRYPTED: &str = "TESSARIDB_FLUSHED_ENCRYPTED";

/// The key an encrypted flushing store runs under.
fn at_rest() -> tessari_lsm::AtRestKey {
    tessari_lsm::AtRestKey::from_key(&tessari_vault::SecretBytes::adopt([11; 32])).unwrap()
}

fn open_small_store(path: &std::path::Path, encrypted: bool) -> Store {
    let config = StoreConfig {
        memtable_bytes: SMALL_MEMTABLE_BYTES,
        ..StoreConfig::new(Durability::PowerLossSafe)
    };
    let key = encrypted.then(at_rest);
    let backend = LsmBackend::open_with_key(path, config, key.as_ref()).unwrap();
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
    let encrypted = std::env::var_os(FLUSHED_ENCRYPTED).is_some();
    let store = open_small_store(std::path::Path::new(&path), encrypted);

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
    killed_after_a_flush(false);
}

/// The same kill on an encrypted store (ADR-0108 D7): recovery reads the
/// encrypted log and tables back, and nothing on disk shows a record.
#[test]
fn an_acknowledged_commit_in_an_encrypted_store_survives_a_kill_after_a_flush() {
    killed_after_a_flush(true);
}

fn killed_after_a_flush(encrypted: bool) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut command = Command::new(std::env::current_exe().unwrap());
    if encrypted {
        command.env(FLUSHED_ENCRYPTED, "1");
    }
    let mut child = command
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

    if encrypted {
        let shown = std::fs::read_dir(&path).unwrap().any(|entry| {
            let file = entry.unwrap().path();
            file.is_file()
                && std::fs::read(&file)
                    .unwrap()
                    .windows(b"payload-".len())
                    .any(|window| window == b"payload-")
        });
        assert!(!shown, "a record is readable in an encrypted store's files");
    }

    let reopened = open_small_store(&path, encrypted);
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
        reopened
            .committed_tail(reopened.own_log(HOME).unwrap())
            .unwrap()
            >= *last,
        "the committed position was recovered behind the records it accounts for"
    );
}

/// Environment variable carrying the store path into the concurrent child.
const GROUPED_STORE_PATH: &str = "TESSARIDB_GROUPED_STORE";
/// Writers committing at once in the concurrent child — enough that commits
/// arrive while one is being synced, so they land in groups.
const GROUPED_WRITERS: u64 = 4;
/// How many acknowledged commits the parent waits for before killing.
const ACKNOWLEDGED_BEFORE_GROUPED_KILL: usize = 80;

/// The concurrent child. Several writers commit forever, each announcing what
/// it was handed; commits that arrive while one is syncing land together
/// (G040 SG4), so the kill can fall in the middle of a group's write.
#[test]
#[ignore = "spawned by the grouped durability test; runs until it is killed"]
fn commit_from_several_writers_until_killed() {
    let Ok(path) = std::env::var(GROUPED_STORE_PATH) else {
        panic!("{GROUPED_STORE_PATH} must name the store directory");
    };
    let store = open_store(std::path::Path::new(&path));
    std::thread::scope(|scope| {
        for writer in 0..GROUPED_WRITERS {
            let store = &store;
            scope.spawn(move || {
                for i in 1..=CHILD_GIVES_UP_AFTER {
                    let n = writer * CHILD_GIVES_UP_AFTER + i;
                    let mut transaction = store.begin().unwrap();
                    transaction.put(address(n), payload(n));
                    let committed = transaction.commit().unwrap();
                    use std::io::Write;
                    let mut stdout = std::io::stdout().lock();
                    writeln!(stdout, "committed {n} {committed}").unwrap();
                    stdout.flush().unwrap();
                }
            });
        }
    });
}

#[test]
fn an_acknowledged_commit_survives_a_kill_among_concurrent_writers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "commit_from_several_writers_until_killed",
            "--ignored",
            "--nocapture",
        ])
        .env(GROUPED_STORE_PATH, &path)
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
            if acknowledged.len() >= ACKNOWLEDGED_BEFORE_GROUPED_KILL {
                break;
            }
        }
    }

    child.kill().unwrap();
    let _ = child.wait();

    assert_eq!(
        acknowledged.len(),
        ACKNOWLEDGED_BEFORE_GROUPED_KILL,
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
    // Acknowledged out of order across writers, so the highest and not the last.
    let highest = acknowledged
        .iter()
        .map(|(_, sequence)| *sequence)
        .max()
        .unwrap();
    assert!(
        reopened
            .committed_tail(reopened.own_log(HOME).unwrap())
            .unwrap()
            >= highest,
        "the committed position was recovered behind the records it accounts for"
    );
}

/// Environment variables carrying the store path and the identity offset into
/// the children of the repeated crash test.
const REPEATED_STORE_PATH: &str = "TESSARIDB_REPEATED_STORE";
const REPEATED_OFFSET: &str = "TESSARIDB_REPEATED_OFFSET";
/// The seed of the repeated crash test; set it to replay a failing run.
const CRASH_SEED: &str = "TESSARIDB_CRASH_SEED";
/// How many times the repeated crash test kills its writer (G059 C4: ≥ 100).
const KILLS: u64 = 100;
/// Every tenth kill is followed by a second kill, of a process recovering.
const RECOVERY_KILL_EVERY: u64 = 10;
/// Records one transaction writes; a reopened store holds all or none of them.
const PARTS: u64 = 3;
/// Identities each writer of one child may use.
const PER_WRITER: u64 = 100_000;

/// The identity writer `writer` of the child at `offset` gives its `i`th transaction.
fn identity(offset: u64, writer: u64, i: u64) -> u64 {
    offset
        .saturating_add(writer.saturating_mul(PER_WRITER))
        .saturating_add(i)
}

fn part(n: u64, of: u64) -> RecordAddress {
    address(n.saturating_mul(PARTS).saturating_add(of))
}

/// A small deterministic generator, so a failing seed replays the same kills.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[test]
#[ignore = "spawned by the repeated crash test; runs until it is killed"]
fn commit_transactions_until_killed() {
    let path = std::env::var(REPEATED_STORE_PATH).expect("the store directory");
    let offset: u64 = std::env::var(REPEATED_OFFSET)
        .expect("the offset")
        .parse()
        .unwrap();
    let store = open_store(std::path::Path::new(&path));
    std::thread::scope(|scope| {
        for writer in 0..GROUPED_WRITERS {
            let store = &store;
            scope.spawn(move || {
                for i in 1..PER_WRITER {
                    let n = identity(offset, writer, i);
                    let mut transaction = store.begin().unwrap();
                    for of in 0..PARTS {
                        transaction.put(part(n, of), payload(n));
                    }
                    transaction.commit().unwrap();
                    use std::io::Write;
                    let mut stdout = std::io::stdout().lock();
                    writeln!(stdout, "committed {writer} {i}").unwrap();
                    stdout.flush().unwrap();
                }
            });
        }
    });
}

#[test]
#[ignore = "spawned by the repeated crash test; opens the store and waits to be killed"]
fn recover_until_killed() {
    let path = std::env::var(REPEATED_STORE_PATH).expect("the store directory");
    let _store = open_store(std::path::Path::new(&path));
    println!("opened");
    loop {
        std::thread::park();
    }
}

fn spawn(test: &str, path: &std::path::Path, offset: u64) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--ignored", "--nocapture"])
        .env(REPEATED_STORE_PATH, path)
        .env(REPEATED_OFFSET, offset.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// G059 C4: a hundred kills at points drawn from a seed, each with four writers
/// committing three-record transactions, and every tenth followed by a kill of
/// the process recovering from it. After each: every transaction acknowledged
/// in any round is present, and every transaction a writer could have reached
/// is present whole or not at all.
#[test]
fn a_hundred_kills_lose_nothing_acknowledged_and_tear_no_transaction() {
    let seed: u64 = std::env::var(CRASH_SEED)
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5eed_0f59);
    let mut state = seed;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let mut acknowledged: Vec<u64> = Vec::new();

    for kill in 0..KILLS {
        let offset = (kill + 1) * GROUPED_WRITERS * PER_WRITER;
        let wanted = usize::try_from(next(&mut state) % 40 + 1).unwrap();
        let mut child = spawn("commit_transactions_until_killed", &path, offset);
        let mut reached = [0u64; 4];
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut read = 0usize;
        for line in lines.by_ref() {
            if record(&line.unwrap(), offset, &mut reached, &mut acknowledged) {
                read += 1;
                if read >= wanted {
                    break;
                }
            }
        }
        child.kill().unwrap();
        // What it announced between the last line read and the kill is a
        // promise too; it is in the pipe.
        for line in lines {
            let Ok(line) = line else { break };
            record(&line, offset, &mut reached, &mut acknowledged);
        }
        let _ = child.wait();
        assert!(
            read >= wanted,
            "seed {seed} kill {kill}: the writer died on its own"
        );

        if kill % RECOVERY_KILL_EVERY == RECOVERY_KILL_EVERY - 1 {
            let mut recovering = spawn("recover_until_killed", &path, 0);
            let spin = std::time::Duration::from_micros(next(&mut state) % 30_000);
            let began = std::time::Instant::now();
            while began.elapsed() < spin {
                std::hint::spin_loop();
            }
            recovering.kill().unwrap();
            let _ = recovering.wait();
        }

        let reopened = open_store(&path);
        let transaction = reopened.begin().unwrap();
        for n in &acknowledged {
            for of in 0..PARTS {
                assert_eq!(
                    transaction.get(&part(*n, of)).unwrap(),
                    Some(payload(*n)),
                    "seed {seed} kill {kill}: acknowledged transaction {n} lost part {of}"
                );
            }
        }
        for (writer, highest) in (0u64..).zip(reached) {
            // One past the last announcement: a commit can return and the
            // writer be killed before it prints.
            for i in 1..=highest.saturating_add(1) {
                let n = identity(offset, writer, i);
                let held = (0..PARTS)
                    .filter(|of| transaction.get(&part(n, *of)).unwrap().is_some())
                    .count();
                assert!(
                    held == 0 || held == usize::try_from(PARTS).unwrap(),
                    "seed {seed} kill {kill}: transaction {n} survived as {held} of {PARTS} records"
                );
            }
        }
    }
    println!(
        "[BGV_CRASH] seed={seed} kills={KILLS} recovery-kills={} acknowledged={}",
        KILLS / RECOVERY_KILL_EVERY,
        acknowledged.len()
    );
}

/// Take one announcement: the transaction it promises and how far its writer
/// reached. `false` for a line that is not one.
fn record(line: &str, offset: u64, reached: &mut [u64; 4], acknowledged: &mut Vec<u64>) -> bool {
    let mut parts = line.split_whitespace();
    if parts.next() != Some("committed") {
        return false;
    }
    let (Some(Ok(writer)), Some(Ok(i))) = (
        parts.next().map(str::parse::<u64>),
        parts.next().map(str::parse::<u64>),
    ) else {
        return false;
    };
    let slot = &mut reached[usize::try_from(writer).unwrap()];
    *slot = (*slot).max(i);
    acknowledged.push(identity(offset, writer, i));
    true
}
