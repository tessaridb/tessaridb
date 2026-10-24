//! `ALTER USER … SET LOCKED` — a user kept, password and grants included, who cannot act until unlocked.
//!
//! The lock is the reversible half of taking somebody's access away. Dropping the user loses the grants and the
//! password with it; rotating the password loses a password its holder was shown once and nobody else kept. A lock
//! keeps both, so `SET UNLOCKED` gives back exactly what was there.
//!
//! Every way a session becomes somebody is tried here, because a lock that only the password path honoured would be
//! a lock with four doors left open: the password, a token cut before the lock, a session already open, a declared
//! consumer acting as the user, and the user changing their own password.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// `root` owns the store, `nina` owns `prod.shop`, `ada` edits there; `staging.lab` holds `lee`, outside nina's reach.
fn peopled(store: &Store) {
    Session::new(store)
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE NAMESPACE staging; USE NAMESPACE staging; DEFINE DATABASE lab;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    signed_in(store, "root")
        .run_now(
            "DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'correct horse battery';\n\
             DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
             DEFINE USER lee ON staging.lab ROLE editor PASSWORD 'correct horse battery';",
        )
        .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).now().unwrap();
    session
}

fn in_shop<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = signed_in(store, name);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// What a sign-in as `name` with `password` answers, as the refusal's words or `ok`.
fn sign_in(store: &Store, name: &str, password: &str) -> String {
    Session::new(store)
        .sign_in(name, password)
        .now()
        .map_or_else(|refused| refused.to_string(), |()| "ok".to_owned())
}

/// The `locked` field `INFO FOR USER` reports.
fn reported_locked(session: &mut Session<'_>, user: &str) -> Value {
    let outcomes = session.run_now(&format!("INFO FOR USER {user};")).unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    report.get("locked").cloned().unwrap_or(Value::None)
}

#[test]
fn a_locked_user_is_refused_as_a_wrong_password_is_and_unlocking_gives_the_same_password_back() {
    let store = store();
    peopled(&store);
    let wrong = sign_in(&store, "ada", "not the password");

    signed_in(&store, "root")
        .run_now("ALTER USER ada SET LOCKED;")
        .unwrap();
    // The right password is refused in the words a wrong one gets: a refusal that said "locked" would tell a guesser
    // they had found the password.
    assert_eq!(sign_in(&store, "ada", PASSWORD), wrong);

    signed_in(&store, "root")
        .run_now("ALTER USER ada SET UNLOCKED;")
        .unwrap();
    assert_eq!(sign_in(&store, "ada", PASSWORD), "ok");
    in_shop(&store, "ada")
        .run_now("CREATE orders:1 = { total: 5 };")
        .expect("the role and the reach come back with the user");
}

#[test]
fn a_lock_reaches_a_session_already_open_on_its_next_statement() {
    let store = store();
    peopled(&store);
    let mut ada = in_shop(&store, "ada");
    ada.run_now("SELECT * FROM orders;")
        .expect("an editor reads");

    signed_in(&store, "root")
        .run_now("ALTER USER ada SET LOCKED;")
        .unwrap();
    let refused = ada
        .run_now("SELECT * FROM orders;")
        .expect_err("the lock did not reach the open session");
    assert!(refused.to_string().contains("signed-in"), "{refused}");
}

#[test]
fn a_token_cut_before_the_lock_is_refused() {
    let store = store();
    peopled(&store);
    let ticket = signed_in(&store, "ada")
        .ticket()
        .expect("a signed-in session cuts a ticket");

    signed_in(&store, "root")
        .run_now("ALTER USER ada SET LOCKED;")
        .unwrap();
    assert!(
        Session::new(&store).resume(&ticket).now().is_err(),
        "a token outlived the lock"
    );
}

#[test]
fn a_locked_user_cannot_change_their_own_password_from_a_session_already_open() {
    let store = store();
    peopled(&store);
    let mut ada = signed_in(&store, "ada");

    signed_in(&store, "root")
        .run_now("ALTER USER ada SET LOCKED;")
        .unwrap();
    assert!(
        ada.change_password(PASSWORD, "a different horse entirely")
            .now()
            .is_err(),
        "a locked user set a new password"
    );
    signed_in(&store, "root")
        .run_now("ALTER USER ada SET UNLOCKED;")
        .unwrap();
    assert_eq!(
        sign_in(&store, "ada", PASSWORD),
        "ok",
        "the old password stands"
    );
}

#[test]
fn locking_is_bounded_by_the_tenancy_the_caller_administers() {
    let store = store();
    peopled(&store);
    let mut nina = signed_in(&store, "nina");
    nina.run_now("ALTER USER ada SET LOCKED;")
        .expect("nina owns ada's database");
    for outside in ["root", "lee"] {
        let refused = nina
            .run_now(&format!("ALTER USER {outside} SET LOCKED;"))
            .expect_err("a database owner locked somebody outside her database");
        assert!(
            refused.to_string().contains("not in a tenancy"),
            "{outside}: {refused}"
        );
    }
    assert_eq!(sign_in(&store, "root", PASSWORD), "ok");
    assert_eq!(sign_in(&store, "lee", PASSWORD), "ok");
}

#[test]
fn info_reports_the_lock_and_a_script_backup_restores_it() {
    let store = store();
    peopled(&store);
    let mut root = signed_in(&store, "root");
    assert_eq!(reported_locked(&mut root, "ada"), Value::Bool(false));
    root.run_now("ALTER USER ada SET LOCKED;").unwrap();
    assert_eq!(reported_locked(&mut root, "ada"), Value::Bool(true));

    let answered = root.run_now("BACKUP SCRIPT;").unwrap();
    let Some(Outcome::Value(Value::String(script))) = answered.last() else {
        panic!("BACKUP SCRIPT answered {answered:?}");
    };
    let restored = self::store();
    Session::new(&restored)
        .run_now(script)
        .unwrap_or_else(|refusal| panic!("the script did not restore: {refusal}\n{script}"));
    assert_ne!(
        sign_in(&restored, "ada", PASSWORD),
        "ok",
        "the restore unlocked ada"
    );
    assert_eq!(sign_in(&restored, "nina", PASSWORD), "ok");
    assert_eq!(
        reported_locked(&mut signed_in(&restored, "root"), "ada"),
        Value::Bool(true)
    );
}
