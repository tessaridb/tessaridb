//! The application-shaped walkthrough, executed through the real binary.
//!
//! # What this file is for
//!
//! Criterion I1 asks that at least one programmatic interface can do everything
//! an application needs, and that **no interface writes a secret into an
//! environment variable, a command line, a URL, or a log**. It asks for the
//! walkthrough — create, write, read, add recipient, remove recipient, destroy —
//! to be *executed end to end*, and for four greps beside it.
//!
//! Until now every one of those steps was exercised at the **session** layer,
//! which is not an interface an application uses. The exfiltration survey's rows
//! 18 and 21 — the CLI renderer and the 35 `log::*` call sites — were
//! dispositioned `tested` in a wave that never ran, and Q-424 records that.
//!
//! # Why the whole binary rather than a library call
//!
//! Because the row is about the **renderer**, and a renderer is the last thing
//! between a value and a person. Calling the session directly would exercise
//! everything except the part this row names. So: the real binary, a real store
//! on disk, a separate operating-system process, and both streams captured —
//! a refusal reaches standard error and an answer reaches standard output, and a
//! test reading one of them would be blind to half the outcomes.
//!
//! # The script arrives on standard input, deliberately
//!
//! `-e` would put the vault's passphrase in this process's argv, and the process
//! table is world-readable. The CLI already refuses to take a *password* that
//! way — its usage text says so — and a vault passphrase is the same kind of
//! thing. Driving the walkthrough over standard input is therefore not an
//! incidental choice of harness: it is the shape an application should copy, and
//! the argv assertion below is what makes that visible rather than implied.

#![allow(clippy::panic, clippy::unwrap_used)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The planted secret. Long and distinctive, so a hit is a hit.
const PLANTED: &str = "correct-horse-battery-staple-9f2b";

/// The passphrase that unseals the store — a secret in its own right, and the
/// one an operator types.
const PASSPHRASE: &str = "an operator passphrase 4b71";

/// What must be found. Without it the scans below pass vacuously.
const CONTROL: &str = "ada-lovelace-marker-71c4";

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("tessaridb")
}

fn store(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("tessaridb-vault-walkthrough-{name}"));
    drop(std::fs::remove_dir_all(&directory));
    directory
}

/// What the child was run with, so the assertions can be about the real thing.
struct Ran {
    ok: bool,
    said: String,
    argv: Vec<String>,
    environment: Vec<String>,
}

/// Run the binary over `store`, feeding `script` on standard input.
///
/// The environment is **cleared** and rebuilt, so what the child holds is
/// exactly what this function put there and the assertion about it means
/// something. Inheriting the test runner's environment would make the grep pass
/// or fail on whatever happened to be exported in the shell that started it.
fn run(store: &Path, script: &str) -> Ran {
    let argv: Vec<String> = vec![store.display().to_string()];
    let environment: Vec<String> = vec![format!("PATH={}", std::env::var("PATH").unwrap())];

    let mut command = Command::new(binary());
    command.env_clear();
    for pair in &environment {
        let (name, value) = pair.split_once('=').unwrap();
        command.env(name, value);
    }
    let mut child = command
        .args(&argv)
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
        argv,
        environment,
    }
}

const TENANCY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                       DEFINE DATABASE work; USE DATABASE work;";

/// What a *later* process needs before it can probe anything.
///
/// The tenancy is already declared, so it is selected rather than declared; and
/// the store is **sealed**, because unsealing is per process and this is a new
/// one. Without the unseal every probe below would be refused for being sealed
/// rather than for its own reason — each scan would still pass, and none of them
/// would be testing the row it names.
fn probing() -> String {
    format!("USE NAMESPACE prod; USE DATABASE work; UNSEAL VAULT WITH '{PASSPHRASE}';")
}

/// Every step criterion I1 names, in one session, through one interface.
///
/// Create the vault, write a record with a sealed field, read it back with
/// `REVEAL`, add a recipient, list them, remove it, and destroy the vault. If
/// any step is missing from the language this fails at that step rather than
/// somewhere convenient.
#[test]
fn the_walkthrough_an_application_needs_runs_through_the_command_line() {
    let store = store("whole");
    let ran = run(
        &store,
        &format!(
            "{TENANCY}
             UNSEAL VAULT WITH '{PASSPHRASE}';
             DEFINE VAULT team;
             DEFINE FIELD login ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;
             CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};
             REVEAL token FROM team:'github';
             ADD RECIPIENT 'ada@example.com' TO team:'github' KEY 0xdeadbeef;
             INFO FOR RECIPIENTS OF team:'github';
             REMOVE RECIPIENT 'ada@example.com' FROM team:'github';
             DROP VAULT team;"
        ),
    );
    assert!(ran.ok, "the walkthrough did not run: {}", ran.said);

    // The one statement that is allowed to answer with a plaintext did.
    assert!(
        ran.said.contains(PLANTED),
        "`REVEAL` did not return the secret, so every scan in this file would \
         pass for the wrong reason: {}",
        ran.said
    );
    // And the recipient came back, so the middle of the walkthrough happened
    // rather than being skipped by an early refusal nobody asserted on.
    assert!(
        ran.said.contains("ada@example.com"),
        "the recipient was not listed: {}",
        ran.said
    );
}

/// Survey row 18 and row 21, and I1's four greps.
///
/// Each probe runs in **its own process**, because this binary stops at the
/// first refusal and most of what is worth probing here is a refusal. A single
/// script would abort at statement one and every scan after it would pass
/// against an empty run — the shape of vacuous success this file exists to
/// avoid.
#[test]
fn nothing_but_reveal_renders_a_secret_and_nothing_writes_one_out_of_band() {
    let store = store("scans");
    let setup = format!(
        "{TENANCY}
         UNSEAL VAULT WITH '{PASSPHRASE}';
         DEFINE VAULT team;
         DEFINE FIELD login ON team TYPE string;
         DEFINE FIELD token ON team TYPE string SECRET;
         CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};
         DEFINE TABLE notes SCHEMALESS;
         CREATE notes:1 = {{ text: '{CONTROL}' }};"
    );
    let ran = run(&store, &setup);
    assert!(ran.ok, "the store would not set up: {}", ran.said);

    // The control run: this renderer does print a value when a statement
    // legitimately answers with one. Without this every scan below could pass
    // because the binary prints nothing at all.
    let control = run(&store, &format!("{} SELECT * FROM notes;", probing()));
    assert!(
        control.said.contains(CONTROL),
        "the renderer printed no value even when asked: {}",
        control.said
    );

    // Grep 1 — the log corpus, which for this binary is both streams together:
    // survey row 18 (the renderer) and row 21 (the 35 `log::*` sites) at once.
    // Each of these either refuses or answers about the vault without opening
    // it, so none of them may produce the secret.
    for probe in [
        "SELECT * FROM team;",
        "SELECT * FROM team:'github';",
        "SELECT * FROM team WHERE token = 'guess';",
        "SELECT * FROM team ORDER BY token;",
        "REVEAL login FROM team:'github';",
        "INFO FOR VAULT team;",
        "INFO FOR TABLE team;",
        "INFO FOR RECIPIENTS OF team:'github';",
        "DEFINE INDEX by_token ON team FIELDS token;",
        "ALTER TABLE team SET SCHEMALESS;",
        "CREATE team:'gitlab' = { login: 'boog', recovery: 'anything' };",
        "SEAL VAULT; REVEAL token FROM team:'github';",
    ] {
        let ran = run(&store, &format!("{} {probe}", probing()));
        assert!(
            !ran.said.contains(PLANTED),
            "`{probe}` put a secret on the command line's output: {}",
            ran.said
        );
        assert!(
            !ran.said.contains(PASSPHRASE),
            "`{probe}` echoed the unseal passphrase: {}",
            ran.said
        );
    }

    // A wrong passphrase is the one statement that carries a secret *in* rather
    // than out, and a refusal that quoted what it was given would publish it.
    let ran = run(
        &store,
        &format!("USE NAMESPACE prod; USE DATABASE work; UNSEAL VAULT WITH 'not-{PASSPHRASE}';"),
    );
    assert!(
        !ran.said.contains(PASSPHRASE),
        "the refusal quoted the passphrase it was given: {}",
        ran.said
    );

    // Grep 2 — argv. The process table is world-readable, so anything here is
    // published to every user on the machine.
    let argv = ran.argv.join(" ");
    assert!(
        !argv.contains(PLANTED) && !argv.contains(PASSPHRASE),
        "a secret is in the process arguments: {argv}"
    );

    // Grep 3 — the environment, which `run` cleared and rebuilt, so this is an
    // assertion about the whole of what the child held rather than about the
    // part the test remembered to look at.
    let environment = ran.environment.join(" ");
    assert!(
        !environment.contains(PLANTED) && !environment.contains(PASSPHRASE),
        "a secret is in the process environment: {environment}"
    );

    // Grep 4 — what landed on disk. There is no URL on this interface; the store
    // directory is its equivalent of a request path, and it is the artifact that
    // outlives the process.
    let mut held = Vec::new();
    walk(&store, &mut held);
    assert!(
        !held.windows(PLANTED.len()).any(|w| w == PLANTED.as_bytes()),
        "the secret is in the store directory in the clear"
    );
    assert!(
        held.windows(CONTROL.len()).any(|w| w == CONTROL.as_bytes()),
        "the control is not on disk either, so the scan above proves nothing"
    );
}

/// Every byte under `directory`, so a scan covers files nobody named.
fn walk(directory: &Path, into: &mut Vec<u8>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, into);
        } else if let Ok(bytes) = std::fs::read(&path) {
            into.extend_from_slice(&bytes);
        }
    }
}

// The dedicated surfaces on a real node (ADR-0092 D2, D4): the passphrase is in
// no line the node writes, whatever the log level, and the period the flag sets
// is the one the node keeps.

const SURFACE_WIRE: &str = "127.0.0.1:47908";
const SURFACE_HTTP: &str = "127.0.0.1:47909";

fn listening(address: &str) -> bool {
    let began = std::time::Instant::now();
    while began.elapsed() < std::time::Duration::from_secs(30) {
        if std::net::TcpStream::connect(address).is_ok() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

/// A node that is killed when the test ends, however it ends. Without it a
/// failed assertion leaves the node holding its ports, and the next run talks
/// to the old process and fails for a reason that has nothing to do with it.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

fn post(address: &str, path: &str, body: &str) -> String {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut said = String::new();
    stream.read_to_string(&mut said).unwrap();
    said
}

#[test]
fn the_passphrase_reaches_no_log_line_and_the_period_is_the_one_asked_for() {
    let path = store("surface");
    let log = std::env::temp_dir().join("tessaridb-vault-surface.log");
    let node = Command::new(binary())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("TESSARIDB_LOG", "trace")
        .args([
            path.display().to_string().as_str(),
            "--serve",
            SURFACE_WIRE,
            "--http",
            SURFACE_HTTP,
            "--unseal-for",
            "1ns",
        ])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let node = Killed(node);
    assert!(
        listening(SURFACE_HTTP) && listening(SURFACE_WIRE),
        "the node never listened"
    );

    // A period of one nanosecond is over before the answer is written, so the
    // store reports itself sealed the moment it was unsealed — which is the
    // flag, applied, observed from outside the process.
    let first = post(SURFACE_HTTP, "/vault/unseal", PASSPHRASE);
    assert!(first.contains(r#""initialised":true"#), "{first}");
    assert!(
        first.contains(r#""state":"sealed""#),
        "the period was not applied: {first}"
    );
    assert!(first.contains(r#""unseal_for":"1ns""#), "{first}");

    let guess = "a guess that is not it 5d20";
    let refused = post(SURFACE_HTTP, "/vault/unseal", guess);
    assert!(!refused.contains(guess), "{refused}");

    let mut client = tessari_wire::Client::connect(SURFACE_WIRE).unwrap();
    client
        .vault(
            &tessari_wire::VaultCall::Unseal(PASSPHRASE.to_owned()),
            None,
        )
        .unwrap();
    drop(client.vault(&tessari_wire::VaultCall::Unseal(guess.to_owned()), None));

    drop(node);
    let written = std::fs::read_to_string(&log).unwrap();
    // The control: the log was captured at a level that records these acts, so
    // the absences below are about the passphrase and not about an empty file.
    assert!(
        written.contains("unseal refused: wrong passphrase"),
        "the log holds no record of the refusal, so the scan proves nothing:\n{written}"
    );
    assert!(
        !written.contains(PASSPHRASE),
        "the passphrase is in the node's log"
    );
    assert!(
        !written.contains(guess),
        "a guessed passphrase is in the node's log"
    );
    drop(std::fs::remove_dir_all(&path));
    drop(std::fs::remove_file(&log));
}
