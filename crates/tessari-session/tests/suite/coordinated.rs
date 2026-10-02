//! A script another node carried here for its caller (ADR-0108 D2).

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::Parameters;
use tessari_session::{Error, Session};
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work; USE DATABASE work;
             DEFINE COLLECTION t; CREATE t:1 = {{ n: 1 }};
             DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE work;
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    store
}

fn acting<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

#[test]
fn a_carried_script_runs_in_the_selection_its_caller_had() {
    let store = store();
    let mut root = acting(&store, "root");
    let ran = root
        .run_coordinated(
            (Some("prod"), Some("work")),
            "SELECT * FROM t; CREATE t:2 = { n: 2 };",
            &Parameters::new(),
        )
        .unwrap();
    // One answer per statement the caller sent: the built `USE` answers nobody.
    assert_eq!(ran.len(), 2, "{ran:?}");
    assert_eq!(root.namespace(), Some("prod"));
    assert!(
        format!("{:?}", root.run("SELECT * FROM t:2;").unwrap()).contains("2"),
        "the carried write did not land"
    );
}

#[test]
fn a_carried_script_that_changes_authority_or_membership_is_refused_whole() {
    let store = store();
    for statement in [
        format!("DEFINE USER eve ROLE owner PASSWORD '{PASSWORD}'"),
        "GRANT read ON t TO root".to_owned(),
        "ALTER USER root SET ROLE viewer".to_owned(),
        "DROP USER root".to_owned(),
        "DEFINE REPLICA n AT '127.0.0.1:1' ROLES serving".to_owned(),
        "DEFINE NODE ROLES serving".to_owned(),
        "BACKUP".to_owned(),
    ] {
        let mut root = acting(&store, "root");
        let refused = root
            .run_coordinated(
                (Some("prod"), Some("work")),
                &format!("CREATE t:9 = {{ n: 9 }}; {statement};"),
                &Parameters::new(),
            )
            .unwrap_err();
        assert!(
            matches!(refused, Error::MayNotTravel { .. }),
            "`{statement}` travelled: {refused:?}"
        );
        // Whole: the write in front of it did not run either.
        assert!(
            !format!(
                "{:?}",
                root.run("USE NAMESPACE prod; USE DATABASE work; SELECT * FROM t;")
                    .unwrap()
            )
            .contains("9"),
            "part of a refused script ran"
        );
    }
}

#[test]
fn the_selection_a_peer_sends_is_a_name_and_never_syntax() {
    // A payload that would WRITE if it were ever read as statements, so the
    // assertion sees an injection rather than a refusal of one.
    let store = store();
    let mut root = acting(&store, "root");
    let _ = root.run_coordinated(
        (
            Some("prod; USE DATABASE work; CREATE t:66 = { n: 66 }"),
            None,
        ),
        "SELECT * FROM t;",
        &Parameters::new(),
    );
    let held = format!(
        "{:?}",
        root.run("USE NAMESPACE prod; USE DATABASE work; SELECT * FROM t;")
            .unwrap()
    );
    // The control: the table answers, with the record the fixture wrote.
    assert!(held.contains("Int(1)"), "{held}");
    assert!(
        !held.contains("66"),
        "a namespace name became a statement: {held}"
    );
}

#[test]
fn a_user_confined_elsewhere_is_stopped_at_the_selection() {
    let store = store();
    let mut root = acting(&store, "root");
    root.run(&format!(
        "DEFINE USER ada ON other.work ROLE owner PASSWORD '{PASSWORD}';"
    ))
    .unwrap();
    let mut ada = acting(&store, "ada");
    // Named, so a refusal for any other reason — no selection at all — cannot
    // pass for the confinement.
    let refused = ada
        .run_coordinated(
            (Some("prod"), Some("work")),
            "SELECT * FROM t;",
            &Parameters::new(),
        )
        .unwrap_err();
    assert!(
        matches!(refused, Error::OutsideTenancy { .. }),
        "a carried request was not stopped at the tenancy: {refused:?}"
    );
}
