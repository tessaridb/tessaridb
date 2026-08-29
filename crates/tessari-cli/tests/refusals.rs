//! The command line is an enforcement point, and it is probed as one.
//!
//! # Why a test per mode rather than a test per handler
//!
//! Because the three script modes — standard input, `-e`, `-f` — are three
//! *paths* that happen to meet at one session, and a coverage matrix counts
//! paths. The argument that they share code is true today and is exactly what a
//! fourth mode added next month would quietly stop satisfying, which is why "the
//! same code is tested elsewhere" was recorded in the matrix as an argument and
//! not as a test.
//!
//! Each probe is the canonical shape: run the protected path **directly**, as a
//! low-privilege identity, with no convenience in the loop — here that means the
//! real binary, a real store on disk, and a write that the identity may not
//! perform. The same statement is then run as the owner, so a refusal that came
//! from a broken store rather than from the permission model would not pass.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const PASSWORD: &str = "correct horse battery";

/// The binary under test.
fn binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("tessaridb")
}

/// A store directory of this test's own, emptied first.
///
/// No port is opened by any mode here, so these run in parallel with everything
/// else — the fixed-port suites in this workspace are the ones that cannot.
fn store(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("tessaridb-cli-refusals-{name}"));
    drop(std::fs::remove_dir_all(&directory));
    directory
}

/// Run the binary over `store`, with `arguments`, feeding `input` to it.
///
/// Returns the exit status and everything it said, both streams together: a
/// refusal reaches standard error and an answer reaches standard output, and a
/// test that read only one of them would be blind to half the outcomes.
fn run(store: &PathBuf, user: Option<&str>, arguments: &[&str], input: &str) -> (bool, String) {
    let mut command = Command::new(binary());
    command.arg(store);
    if let Some(name) = user {
        // Per child rather than per process: `set_var` is shared across threads
        // and these tests run beside each other.
        command
            .arg("--user")
            .arg(name)
            .env("TESSARIDB_PASSWORD", PASSWORD);
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
        .write_all(input.as_bytes())
        .unwrap();
    let finished = child.wait_with_output().unwrap();
    let mut said = String::from_utf8_lossy(&finished.stdout).into_owned();
    said.push_str(&String::from_utf8_lossy(&finished.stderr));
    (finished.status.success(), said)
}

/// A store holding a tenancy, a table, an owner and a viewer.
fn peopled(store: &PathBuf) {
    let (ok, said) = run(
        store,
        None,
        &[
            "-e",
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders; \
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        ],
        "",
    );
    assert!(ok, "the store would not set up: {said}");
    let (ok, said) = run(
        store,
        Some("root"),
        &[
            "-e",
            "USE NAMESPACE prod; USE DATABASE shop; \
             DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
        ],
        "",
    );
    assert!(ok, "the viewer would not be declared: {said}");
}

/// The write every probe attempts, and the tenancy it needs first.
const WRITING: &str = "USE NAMESPACE prod; USE DATABASE shop; CREATE orders:1 = { total: 5 };";

#[test]
fn a_viewer_is_refused_the_write_in_every_script_mode_and_the_owner_is_not() {
    let store = store("modes");
    peopled(&store);

    // `-e`, an inline script.
    let (ok, said) = run(&store, Some("vic"), &["-e", WRITING], "");
    assert!(!ok, "inline: a viewer wrote");
    assert!(said.contains("write"), "inline: {said}");

    // Standard input, which is the mode a person gets by typing.
    let (ok, said) = run(&store, Some("vic"), &[], WRITING);
    assert!(!ok, "standard input: a viewer wrote");
    assert!(said.contains("write"), "standard input: {said}");

    // `-f`, a file, which is the mode a deployment script gets.
    let script = store.join("write.tessariql");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(&script, WRITING).unwrap();
    let (ok, said) = run(&store, Some("vic"), &["-f", script.to_str().unwrap()], "");
    assert!(!ok, "file: a viewer wrote");
    assert!(said.contains("write"), "file: {said}");

    // The same statement, as somebody who may: without this the three refusals
    // above would also be satisfied by a store that had stopped working.
    let (ok, said) = run(&store, Some("root"), &["-e", WRITING], "");
    assert!(ok, "the owner was refused too: {said}");
}

#[test]
fn a_scoped_user_cannot_reach_another_tenancy_from_the_command_line() {
    // The crossing, on the path a deployment script actually takes. It is here
    // rather than only in the session tests because a coverage matrix counts
    // paths, and this one carries its own argument parsing, its own sign-in and
    // its own script source before it ever reaches a statement.
    let store = store("crossing");
    let (ok, said) = run(
        &store,
        None,
        &[
            "-e",
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders; \
             CREATE orders:1 = { total: 5 }; \
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        ],
        "",
    );
    assert!(ok, "the store would not set up: {said}");
    let (ok, said) = run(
        &store,
        Some("root"),
        &[
            "-e",
            "DEFINE NAMESPACE staging; USE NAMESPACE staging; \
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders; \
             CREATE orders:1 = { total: 9 }; \
             USE NAMESPACE prod; USE DATABASE shop; \
             DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'correct horse battery';",
        ],
        "",
    );
    assert!(ok, "the second tenancy would not be built: {said}");

    // Her own tenancy answers, so the refusal below is about the crossing.
    let (ok, said) = run(
        &store,
        Some("nina"),
        &[
            "-e",
            "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;",
        ],
        "",
    );
    assert!(ok, "her own tenancy was refused: {said}");
    assert!(
        said.contains('5'),
        "her own record did not come back: {said}"
    );

    let (ok, said) = run(&store, Some("nina"), &["-e", "USE NAMESPACE staging;"], "");
    assert!(!ok, "she selected another namespace: {said}");
    // On what it says rather than on a digit: the output carries a log line
    // with a timestamp, and asserting a digit is absent reads that timestamp as
    // if it were a record.
    assert!(
        said.contains("outside"),
        "she was refused for some other reason: {said}"
    );
}
