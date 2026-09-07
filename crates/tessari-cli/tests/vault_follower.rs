//! A follower that is a second operating-system process.
//!
//! # The clause this file exists for
//!
//! Criterion E3 reads *the replication log carries ciphertext*, and its
//! validation asks for a follower that **holds the item and cannot open it**,
//! *asserted against a second node*.
//!
//! The first half is already proven at the library level:
//! `tessari-backup/tests/vault_restore.rs` builds a store from the leader's log
//! bytes, holds the record, is refused when it tries to open it, and writes
//! index entries on the apply path — which matters, because a follower replays
//! records rather than copying bytes, so the index writer runs on the follower
//! too.
//!
//! # What a second process adds, and what it does not
//!
//! It does **not** add validity. The reference standard the criterion names —
//! G018's L2 — passed with two stores in one process, and a library-level
//! follower is a legitimate follower.
//!
//! What it adds is that E3's threat is operational: a node somebody else runs,
//! holding a copy of the log, on their own disk. Three things exist only across
//! that boundary. The log is a **file the real binary wrote** rather than a
//! `Vec<u8>` a test built. The follower's bytes are **files on a disk** rather
//! than a memory backend. And the follower process **never received the
//! passphrase**, so it cannot have inherited an unsealed keyring through shared
//! memory — it has no shared memory to inherit one through. The third is the
//! one an in-process test can argue but not demonstrate, because there the
//! sealed store sits in the same address space as the unsealed one.
//!
//! # Two directories and a file — no port
//!
//! Replication travels here as a **log artifact**, not over a socket: the
//! binary carries `--backup <file>`, which writes the store's log and exits,
//! and `--restore <file>`, which replays one into an empty store and exits. So
//! this needs no listening socket, and it does not inherit
//! `tessari-cli/tests/serving.rs`'s fixed ports 47823-47825 or its requirement
//! to run alone. The harness is `vault_walkthrough.rs`'s: the real binary, a
//! cleared environment, and the script on standard input so no passphrase
//! reaches argv.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The planted secret. Long and distinctive, so a hit is a hit.
const PLANTED: &str = "correct-horse-battery-staple-4d19";

/// The passphrase that unseals the store, and a secret in its own right.
const PASSPHRASE: &str = "an operator passphrase 8e02";

/// An ordinary field's value on the **same record**, equally distinctive and
/// not secret.
///
/// Every scan below is paired with this. A search that cannot find a string
/// written in the clear is not searching, and would report a sealed secret as
/// absent from an empty file just as happily as from a full one.
const CONTROL: &str = "grace-hopper-marker-30b8";

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("tessaridb")
}

/// A directory this test owns, emptied first so a previous run cannot answer
/// for this one.
fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tessaridb-vault-follower-{name}"));
    drop(std::fs::remove_dir_all(&path));
    path
}

/// What the child was run with, so the assertions can be about the real thing.
struct Ran {
    ok: bool,
    said: String,
    argv: Vec<String>,
    environment: Vec<String>,
}

/// Run the binary with `arguments`, feeding `script` on standard input.
///
/// The environment is **cleared** and rebuilt, so what the child held is
/// exactly what this function put there — inheriting the test runner's
/// environment would make the scan below pass or fail on whatever happened to
/// be exported in the shell that started `cargo`.
fn run(arguments: &[String], script: &str) -> Ran {
    let environment: Vec<String> = vec![format!("PATH={}", std::env::var("PATH").unwrap())];

    let mut command = Command::new(binary());
    command.env_clear();
    for pair in &environment {
        let (name, value) = pair.split_once('=').unwrap();
        command.env(name, value);
    }
    let mut child = command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let finished = child.wait_with_output().unwrap();
    let mut said = String::from_utf8_lossy(&finished.stdout).into_owned();
    said.push_str(&String::from_utf8_lossy(&finished.stderr));
    Ran {
        ok: finished.status.success(),
        said,
        argv: arguments.to_vec(),
        environment,
    }
}

/// Run the binary over a store with no extra flags.
fn over(store: &Path, script: &str) -> Ran {
    run(&[store.display().to_string()], script)
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Every byte of every file under `path`, concatenated.
///
/// A store is a directory of files whose names and count are the engine's
/// business, so this walks whatever is there rather than naming what it expects
/// to find — a file the engine starts writing tomorrow is covered without
/// anybody remembering to come back here.
fn every_byte_under(path: &Path) -> Vec<u8> {
    let mut held = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                held.extend(std::fs::read(entry.path()).unwrap());
            }
        }
    }
    assert!(
        !held.is_empty(),
        "there is nothing under {}",
        path.display()
    );
    held
}

const TENANCY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                       DEFINE DATABASE work; USE DATABASE work;";

/// The leader's whole life: declare a vault, seal a secret into it, and write
/// the log a follower will be built from.
///
/// The unseal precedes the `DEFINE VAULT`, which is the order the language
/// requires — a vault cannot be declared by a session that cannot reach the
/// master key.
fn leader(store: &Path, log: &Path) -> Vec<Ran> {
    let wrote = over(
        store,
        &format!(
            "{TENANCY}
             UNSEAL VAULT WITH '{PASSPHRASE}';
             DEFINE VAULT team;
             DEFINE FIELD login ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;
             CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};"
        ),
    );
    assert!(wrote.ok, "the leader would not set up: {}", wrote.said);

    let backed_up = run(
        &[
            store.display().to_string(),
            "--backup".to_owned(),
            log.display().to_string(),
        ],
        "",
    );
    assert!(
        backed_up.ok,
        "the leader would not write its log: {}",
        backed_up.said
    );
    vec![wrote, backed_up]
}

/// A second node, built only by replaying the leader's log into an empty
/// directory.
fn follower(store: &Path, log: &Path) {
    let restored = run(
        &[
            store.display().to_string(),
            "--restore".to_owned(),
            log.display().to_string(),
        ],
        "",
    );
    assert!(
        restored.ok,
        "the follower would not take the log: {}",
        restored.said
    );
}

/// E3, both clauses, against a node in its own process.
///
/// The refusals are asserted by the words only they can produce. A `REVEAL`
/// refused because the store is sealed and a `REVEAL` refused because the key
/// is wrong are different failures, and a test that accepted either for the
/// other would pass while the follower opened secrets with any passphrase at
/// all.
#[test]
fn a_follower_in_its_own_process_holds_the_item_and_cannot_open_it() {
    let leader_store = directory("leader");
    let follower_store = directory("follower");
    let log = std::env::temp_dir().join("tessaridb-vault-follower.log");
    drop(std::fs::remove_file(&log));

    drop(leader(&leader_store, &log));
    follower(&follower_store, &log);

    // It holds the item. `SELECT` cannot show this — a vault refuses to be read
    // that way, which is itself part of the design — so the vault and the shape
    // of the record it carries are read from the catalog the log delivered.
    let described = over(
        &follower_store,
        "USE NAMESPACE prod; USE DATABASE work; INFO FOR VAULT team;",
    );
    assert!(
        described.ok,
        "the follower has no vault: {}",
        described.said
    );
    assert!(
        described.said.contains("secret: true"),
        "the follower does not know the field is secret, so it did not receive \
         the declaration that seals it: {}",
        described.said
    );

    // And it cannot open it. Sealed first: this process was handed a store and
    // nothing else.
    let sealed = over(
        &follower_store,
        "USE NAMESPACE prod; USE DATABASE work; REVEAL token FROM team:'github';",
    );
    assert!(
        sealed.said.contains("the vault is sealed"),
        "a fresh follower answered a REVEAL without being unsealed: {}",
        sealed.said
    );
    assert!(
        !sealed.said.contains(PLANTED),
        "the refusal carried the secret: {}",
        sealed.said
    );

    // Then with a passphrase that is wrong, which fails for a different reason
    // and must say so — otherwise the assertion above could be passing because
    // every REVEAL on this node fails for one blanket reason.
    let wrong = over(
        &follower_store,
        "USE NAMESPACE prod; USE DATABASE work;
         UNSEAL VAULT WITH 'not the passphrase';
         REVEAL token FROM team:'github';",
    );
    assert!(
        !wrong.said.contains(PLANTED),
        "a wrong passphrase opened the secret: {}",
        wrong.said
    );
    assert!(
        wrong.said.contains("the key is wrong"),
        "the wrong passphrase was not refused for being wrong: {}",
        wrong.said
    );
    assert!(
        !wrong.said.contains("the vault is sealed"),
        "the wrong passphrase was refused for being sealed rather than for \
         being wrong, so the two refusals are not distinguished: {}",
        wrong.said
    );

    // The narrowed claim, asserted rather than implied. The wrapped key travels
    // in the log, so a follower CAN open the vault with the passphrase and
    // nothing else — Q-423. A file that stopped at the two refusals above would
    // read as if it could not, which is the opposite of what was measured.
    let opened = over(
        &follower_store,
        &format!(
            "USE NAMESPACE prod; USE DATABASE work;
             UNSEAL VAULT WITH '{PASSPHRASE}';
             REVEAL token FROM team:'github';"
        ),
    );
    assert!(
        opened.said.contains(PLANTED),
        "the passphrase did not open the vault on the follower, so this test \
         does not know whether the two refusals above mean anything: {}",
        opened.said
    );
}

/// What the log and the follower's disk actually carry.
///
/// Separate from the test above because it asks a different question of a
/// different thing: not what the node answers, but what is written down.
#[test]
fn neither_the_log_nor_the_followers_disk_carries_the_secret() {
    let leader_store = directory("bytes-leader");
    let follower_store = directory("bytes-follower");
    let log = std::env::temp_dir().join("tessaridb-vault-follower-bytes.log");
    drop(std::fs::remove_file(&log));

    let leading = leader(&leader_store, &log);
    follower(&follower_store, &log);

    // The log artifact, which is the criterion's own subject.
    let carried = std::fs::read(&log).unwrap();
    assert!(
        holds(&carried, CONTROL),
        "the log does not carry an ordinary field written in the clear, so a \
         scan of it finding no secret would prove nothing"
    );
    assert!(
        !holds(&carried, PLANTED),
        "the replication log carries the secret in the clear"
    );

    // The follower's own files, which is where the secret would rest if the
    // apply path unsealed anything on the way in.
    let written = every_byte_under(&follower_store);
    assert!(
        holds(&written, CONTROL),
        "the follower's files carry no ordinary value either, so the scan is \
         looking at a store that never received the record"
    );
    assert!(
        !holds(&written, PLANTED),
        "the follower's disk holds the secret in the clear"
    );

    // Neither the passphrase nor the secret reached the process table or the
    // environment — scanned on the **leader's** invocations, because those are
    // the ones that carried both. Scanning a process that was never given a
    // secret would assert nothing and would look identical in the output.
    //
    // The passphrase is scanned for beside the secret because it arrives by
    // statement, and a node that published it would hand over every secret the
    // vault holds rather than one.
    for ran in &leading {
        for held in ran.argv.iter().chain(ran.environment.iter()) {
            assert!(
                !held.contains(PLANTED) && !held.contains(PASSPHRASE),
                "a secret reached the process table or the environment: {held}"
            );
        }
    }
    // And neither reached what the leader printed. `--backup` reports what it
    // wrote and the write reports `ok`; a renderer that echoed the statement it
    // ran would publish the passphrase to whatever collects that output.
    for ran in &leading {
        assert!(
            !ran.said.contains(PASSPHRASE) && !ran.said.contains(PLANTED),
            "the leader printed a secret: {}",
            ran.said
        );
    }
}
