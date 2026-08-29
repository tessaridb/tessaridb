//! Statements whose subject is the **store**, and who may run them.
//!
//! # The hole this file exists to hold shut
//!
//! A role says *what kind of act* a caller may perform. A tenancy says *where*.
//! For every statement that names a namespace, those two are checked separately
//! and the second one catches an owner of one database reaching into another.
//!
//! A handful of statements name **no tenancy at all**, because their subject is
//! the whole store or the machine serving it. For those, the tenancy check has
//! nothing to look at and passes over them — so before this file, an owner of a
//! single database satisfied `Administer` and `BACKUP` handed them every record
//! in every namespace. Not a theoretical reach: the backup was taken, and the
//! other namespace's data was in the bytes.
//!
//! That is the same shape as the five holes wave J closed, in the one place
//! those fixes could not reach: they bounded the *subject* of a statement
//! against the caller's tenancy, and these statements have no subject to bound.
//!
//! So the rule here is about the caller instead — **an owner with no tenancy of
//! their own**, which is the only identity whose reach is the whole store.
//!
//! # Why a namespace is on this list
//!
//! `DEFINE NAMESPACE` creates a **sibling**, and nothing contains a sibling. An
//! owner of `prod` creating `billing` is not extending their own tenancy, and an
//! *editor* doing it — which is what the old classification allowed, since
//! defining structure is writing — is a caller with no authority over any
//! tenancy adding one to the store's top-level list.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two namespaces with a record each, a store owner, and three users inside one
/// database of one of them.
///
/// `secret.vault` exists so that "may this caller reach the whole store" has a
/// concrete answer: a record they must never see, in a namespace they cannot
/// name.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders; CREATE orders:1 = { total: 5 };",
        )
        .unwrap();
    session
        .run(
            "DEFINE NAMESPACE secret; USE NAMESPACE secret;\n\
             DEFINE DATABASE vault; USE DATABASE vault;\n\
             DEFINE TABLE holdings; CREATE holdings:1 = { value: 'the crown jewels' };",
        )
        .unwrap();
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = signed_in(store, "root");
    root.run(
        "DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'correct horse battery';\n\
         DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// A session for `name`, selected onto the tenancy they hold.
fn inside<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = signed_in(store, name);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// Every statement whose subject is the store rather than a tenancy.
///
/// Listed once and used by every test below, so a statement added to the set is
/// added to the whole matrix rather than to whichever test somebody remembered.
const STORE_WIDE: &[&str] = &[
    "BACKUP;",
    "INFO FOR NODE;",
    "SELECT * FROM $node;",
    "EXPLAIN SELECT * FROM $node;",
    "DEFINE NAMESPACE somewhere_else;",
];

#[test]
fn a_database_owner_may_run_none_of_them() {
    let store = store();
    peopled(&store);

    for statement in STORE_WIDE {
        let mut nina = inside(&store, "nina");
        assert!(
            nina.run(statement).is_err(),
            "an owner of one database ran {statement:?}, whose subject is the whole store"
        );
    }
}

#[test]
fn an_editor_may_run_none_of_them_either() {
    let store = store();
    peopled(&store);

    // `ada` is an editor **of one database**, which is the case that matters:
    // she fails the role check on most of these and the *reach* check on
    // `DEFINE NAMESPACE`, where the role would have let her through.
    for statement in STORE_WIDE {
        let mut ada = inside(&store, "ada");
        assert!(
            ada.run(statement).is_err(),
            "an editor of one database ran {statement:?}, whose subject is the whole store"
        );
    }
}

#[test]
fn the_store_owner_may_run_all_of_them() {
    let store = store();
    peopled(&store);

    // The other half of the rule, and the one that makes it a boundary rather
    // than a blanket refusal: somebody has to be able to back the store up.
    for statement in STORE_WIDE {
        let mut root = signed_in(&store, "root");
        root.run(statement).unwrap_or_else(|failure| {
            panic!("the store's owner could not run {statement:?}: {failure}")
        });
    }
}

#[test]
fn a_backup_taken_by_a_database_owner_would_have_held_another_namespace() {
    let store = store();
    peopled(&store);

    // The measurement that made this a defect rather than a preference. Before
    // the fix this call succeeded and the bytes contained `the crown jewels`
    // from a namespace `nina` cannot name, let alone read.
    let mut nina = inside(&store, "nina");
    assert!(
        nina.run("BACKUP;").is_err(),
        "a database owner backed up the store"
    );

    // And the store's owner still gets those bytes, so the test is about who
    // rather than about whether backups work.
    let mut root = signed_in(&store, "root");
    let taken = root.run("BACKUP;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Bytes(bytes))) = taken.last() else {
        panic!("a backup with no bytes");
    };
    let text = String::from_utf8_lossy(bytes);
    assert!(
        text.contains("the crown jewels"),
        "the store owner's backup should hold every namespace"
    );
}

#[test]
fn there_are_exactly_two_reaches_a_user_can_be_given() {
    let store = store();
    peopled(&store);

    // Worth asserting because the containment rule reads as though there were
    // three. `ON` takes a table reference, so a lone name is a **database** in
    // the selected namespace — `ON prod` is not "the namespace prod", it is
    // "the database prod", and there is no syntax for a namespace-wide user.
    //
    // So the two reaches are the whole store and one database, and the check
    // that guards the store-wide statements — "does this user hold a tenancy of
    // their own" — is exactly the line between them.
    let mut root = signed_in(&store, "root");
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    let refusal = root
        .run("DEFINE USER pat ON prod ROLE owner PASSWORD 'correct horse battery';")
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("no database named"),
        "`ON <one name>` should be read as a database: {refusal}"
    );
}

#[test]
fn a_store_wide_editor_may_declare_a_namespace_and_still_not_back_the_store_up() {
    let store = store();
    peopled(&store);

    // Role and reach are two axes, and this is the case that proves it: an
    // editor whose reach *is* the store may add a namespace, because a
    // namespace is less than the databases and tables they can already define
    // anywhere. What they may not do is administer it.
    //
    // The first draft of this rule collapsed the two and refused them the
    // namespace, which an existing test caught — `DEFINE NAMESPACE` needs a
    // store-wide *reach*, not a raised role.
    let mut root = signed_in(&store, "root");
    root.run("DEFINE USER wide ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();

    let mut wide = signed_in(&store, "wide");
    wide.run("DEFINE NAMESPACE editors_may;").unwrap();
    assert!(
        wide.run("BACKUP;").is_err(),
        "an editor backed up the store"
    );
    assert!(wide.run("INFO FOR NODE;").is_err());
}

#[test]
fn the_refusal_says_it_is_about_reach_and_not_about_role() {
    let store = store();
    peopled(&store);

    // An owner told "you are not an owner" goes looking for the wrong thing.
    // The two refusals have different fixes — be made an owner, against be made
    // an owner *of the store* — so they must not read alike.
    let mut nina = inside(&store, "nina");
    let refusal = nina.run("BACKUP;").unwrap_err().to_string();
    assert!(
        !refusal.contains("may not administer"),
        "an owner of a part was told they are not an owner: {refusal}"
    );
    assert!(
        refusal.contains("whole store"),
        "the refusal should name what is missing: {refusal}"
    );
    // And it says nothing about roles, because for `DEFINE NAMESPACE` the role
    // is not what is missing — a store-wide editor may run that one.
    assert!(!refusal.contains("owner"), "{refusal}");

    // A viewer asking for the same thing is a different refusal, because for
    // them the missing thing really is an authority they hold nowhere. It is
    // `operate` and not `read`: a backup demands both, and the one they are
    // short of is the one worth naming.
    let mut vic = inside(&store, "vic");
    let refusal = vic.run("BACKUP;").unwrap_err().to_string();
    assert!(
        refusal.contains("operate"),
        "a viewer should still be told which authority is missing: {refusal}"
    );
}

#[test]
fn an_open_store_may_still_declare_its_first_namespace() {
    // The exemption that has to survive: every deployment begins by declaring a
    // namespace against a store that has no users, and a rule requiring a
    // store-wide owner would make an empty store impossible to set up.
    let store = store();
    let mut anybody = Session::new(&store);
    anybody
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop;")
        .unwrap();
}

#[test]
fn what_a_database_owner_may_still_do_is_unchanged() {
    let store = store();
    peopled(&store);

    // The other side of a boundary test, and the one that catches a fix which
    // went too far: everything inside their own tenancy still works.
    let mut nina = inside(&store, "nina");
    nina.run("CREATE orders:2 = { total: 9 };").unwrap();
    nina.run("SELECT * FROM orders;").unwrap();
    nina.run("INFO FOR STORE;").unwrap();
    nina.run("INFO FOR USERS;").unwrap();
    nina.run("DEFINE USER extra ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .unwrap();
    nina.run("DEFINE DATABASE another;").unwrap();
}

#[test]
fn info_for_store_still_shows_only_what_the_caller_holds() {
    let store = store();
    peopled(&store);

    // Already true before this wave, and asserted here because it is the
    // property that made `BACKUP` a leak rather than a duplicate of something
    // already visible: `nina` cannot even see that `secret` exists.
    let mut nina = inside(&store, "nina");
    let said = format!("{:?}", nina.run("INFO FOR STORE;").unwrap());
    assert!(said.contains("prod"), "{said}");
    assert!(
        !said.contains("secret"),
        "a database owner saw a sibling namespace: {said}"
    );
}
