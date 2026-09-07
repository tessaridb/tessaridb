//! Reads of a vault are recorded before their answer leaves, or refused.
//!
//! # What each of these can and cannot prove
//!
//! **That the record is written first** cannot be observed directly from
//! outside — the two orderings are indistinguishable whenever nothing fails,
//! which is the whole reason the defect survives review. So it is proved by
//! making something fail: a `REVEAL` inside a transaction the caller then
//! cancels. If the record rode along in the reader's transaction it would roll
//! away with it, and the plaintext would already have been returned. The trail
//! surviving that cancellation is the observable the ordering leaves behind.
//!
//! **That a broken trail refuses** is proved by breaking one. A device is
//! installed that always fails, and the read must come back as an error — not as
//! a plaintext with a warning somewhere.
//!
//! **That the trail holds no value** is proved by planting a distinctive secret,
//! revealing it, and scanning every entry for it, with a control asserting the
//! scan would have found the string had it been there. Without the control an
//! empty trail and a clean one look identical.
//!
//! **That the forensic question is answerable** is proved by asking it against a
//! populated trail and timing it. A question that is answerable in principle and
//! impractical in fact is the usual failure, and only the clock tells them
//! apart.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::{AuditDevice, Store, VaultRead};
use tessari_types::Value;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
";

const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// A device that cannot record anything, ever.
#[derive(Debug)]
struct Broken;

impl AuditDevice for Broken {
    fn record(&self, _event: &VaultRead<'_>) -> Result<(), String> {
        Err("this device is deliberately broken".to_owned())
    }
}

/// A device that keeps what it was handed, so the ordering can be inspected.
#[derive(Debug, Default)]
struct Watching {
    seen: std::sync::Mutex<Vec<(String, String, bool)>>,
}

impl AuditDevice for Watching {
    fn record(&self, event: &VaultRead<'_>) -> Result<(), String> {
        self.seen.lock().unwrap().push((
            event.actor.to_owned(),
            event.record.to_owned(),
            event.served,
        ));
        Ok(())
    }
}

fn holding() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "{TENANCY}
                 UNSEAL VAULT WITH 'an operator passphrase';
                 DEFINE VAULT team;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 CREATE team:'github' = {{ token: '{PLANTED}' }};"
            ))
            .unwrap();
    }
    store
}

fn session_on(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(USING).unwrap();
    session
}

/// Every entry in the trail, as one blob of text to search.
fn trail_text(store: &Store) -> String {
    tessari_storage::audit_entries(store)
        .unwrap()
        .iter()
        .map(|entry| format!("{entry:?}"))
        .collect()
}

#[test]
fn a_read_leaves_a_record_naming_who_and_what() {
    let store = holding();
    let mut session = session_on(&store);
    session.run("REVEAL token FROM team:'github';").unwrap();

    let entries = tessari_storage::audit_entries(&store).unwrap();
    assert_eq!(entries.len(), 1, "expected exactly one recorded read");
    let Value::Object(entry) = &entries[0] else {
        panic!("the entry is not an object");
    };
    assert_eq!(entry.get("actor"), Some(&Value::String("anonymous".into())));
    assert_eq!(entry.get("vault"), Some(&Value::String("team".into())));
    assert_eq!(
        entry.get("record"),
        Some(&Value::String("'github'".to_owned())),
    );
    assert_eq!(entry.get("served"), Some(&Value::Bool(true)));
    assert_eq!(
        entry.get("fields"),
        Some(&Value::Array(vec![Value::String("token".into())])),
    );
    assert!(entry.contains_key("at"), "no time on the entry");
}

#[test]
fn the_record_survives_a_transaction_the_reader_cancels() {
    let store = holding();
    let mut session = session_on(&store);

    // The plaintext left this statement. If the record of it had ridden along in
    // the reader's own transaction, `CANCEL` would have taken it away — and the
    // store would have served a secret with nothing anywhere to say so.
    session
        .run("BEGIN; REVEAL token FROM team:'github'; CANCEL;")
        .unwrap();

    assert_eq!(
        tessari_storage::audit_entries(&store).unwrap().len(),
        1,
        "the record was rolled away with the reader's transaction",
    );
}

#[test]
fn a_read_that_cannot_be_recorded_is_refused() {
    let store = holding();
    store.audit().install(Arc::new(Broken));
    let mut session = session_on(&store);

    let refused = session
        .run("REVEAL token FROM team:'github';")
        .expect_err("the read was served with a broken trail");
    let message = refused.to_string();
    assert!(message.contains("recorded"), "{message}");

    // And the refusal is not a quiet downgrade: nothing came back at all.
    assert!(
        !message.contains(PLANTED),
        "the refusal quoted the secret it refused to serve",
    );
}

#[test]
fn a_refused_read_is_recorded_too() {
    let store = holding();
    let watching = Arc::new(Watching::default());
    store
        .audit()
        .install(Arc::clone(&watching) as Arc<dyn AuditDevice>);
    let mut session = session_on(&store);

    session.run("SEAL VAULT;").unwrap();
    session
        .run("REVEAL token FROM team:'github';")
        .expect_err("a sealed store served a secret");

    // A denial is the reconnaissance signal, and a trail that keeps only
    // successes shows nothing until the damage is done.
    let seen = watching.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "'github'");
    assert!(!seen[0].2, "the refused read was recorded as served");
}

#[test]
fn the_trail_records_identity_and_never_value() {
    let store = holding();
    let mut session = session_on(&store);
    let opened = session.run("REVEAL token FROM team:'github';").unwrap();
    // The control: the secret really was served, so a clean trail below is a
    // trail that withheld it rather than one describing a read that never
    // happened.
    assert!(
        format!("{opened:?}").contains(PLANTED),
        "the read did not return the planted secret, so the scan proves nothing",
    );

    let trail = trail_text(&store);
    assert!(
        !trail.contains(PLANTED),
        "the audit trail holds the secret it recorded a read of",
    );
    // The second control: the scan is looking at something, and would have found
    // the string had it been there.
    assert!(trail.contains("token"), "the trail is empty");
    assert!(
        trail_text(&store).contains("github"),
        "the trail does not name the record",
    );
}

#[test]
fn the_forensic_question_is_answered_against_real_data_and_timed() {
    let store = holding();
    {
        let mut owner = session_on(&store);
        owner
            .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
            .unwrap();
        let mut root = Session::new(&store);
        root.sign_in("root", "correct horse battery").unwrap();
        root.run(&format!(
            "{USING}
             DEFINE USER ada ON prod.work ROLE editor PASSWORD 'correct horse battery';
             GRANT read, write ON team TO ada;"
        ))
        .unwrap();
    }

    // Two credentials reading, interleaved, so the answer has to separate them
    // rather than return everything.
    for name in ["ada", "root"] {
        for _ in 0..25 {
            let mut session = Session::new(&store);
            session.sign_in(name, "correct horse battery").unwrap();
            session.run(USING).unwrap();
            session.run("REVEAL token FROM team:'github';").unwrap();
        }
    }

    // *This credential was compromised; what did it read?*
    let started = Instant::now();
    let reads = tessari_storage::reads_by(&store, "ada").unwrap();
    let took = started.elapsed();

    assert_eq!(reads.len(), 25, "the answer is not ada's reads alone");
    for read in &reads {
        let Value::Object(entry) = read else {
            panic!("not an object");
        };
        assert_eq!(entry.get("actor"), Some(&Value::String("ada".into())));
        assert_eq!(entry.get("vault"), Some(&Value::String("team".into())));
    }

    // Measured rather than asserted possible. The number is small because the
    // trail is, and the scan behind it is the limit named in `audit.rs` — the
    // point of timing it here is that the question runs at all, on real entries,
    // rather than being described as answerable.
    println!("the forensic question over 50 entries took {took:?}");
    assert!(
        took < std::time::Duration::from_secs(5),
        "the forensic question took {took:?}, which is not a usable answer",
    );
}
