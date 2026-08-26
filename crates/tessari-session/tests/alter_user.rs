//! `ALTER USER` — changing one thing about somebody who already exists.
//!
//! Two properties are worth a file of their own.
//!
//! **Only the named field moves.** A password rotation that also reset a role,
//! or a role correction that also invalidated a password, would be discovered
//! by the person locked out rather than by the person who ran the statement. So
//! every test here changes one field and then *exercises the other one* — signs
//! in with the untouched password, runs a statement the untouched role permits.
//! Reading the catalog back would assert the same value the executor just wrote
//! and prove nothing about whether it still works.
//!
//! **Owning something is not owning everything.** The `Administer` check that
//! runs before the executor says the caller owns *a* tenancy. On its own that
//! would let the owner of one database set the store owner's password and take
//! the node — an escalation that looks exactly like routine administration in
//! every log it appears in. The containment check is what closes it, and
//! `a_database_owner_cannot_reach_the_store_owner` is the test that holds it
//! shut.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";
const ROTATED: &str = "a different horse entirely";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A store with a tenancy, a store owner, a database owner, and two others.
///
/// `root` owns the store. `nina` owns `prod.shop` and nothing above it, which
/// is the position the escalation tests are run from. `ada` edits and `vic`
/// views, both inside `prod.shop`.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;",
        )
        .unwrap();
    // The first user closes the store, so the rest are declared by the owner.
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
    signed_in_with(store, name, PASSWORD)
}

fn signed_in_with<'a>(store: &'a Store, name: &str, password: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, password).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// Whether this name and password are accepted right now.
fn signs_in(store: &Store, name: &str, password: &str) -> bool {
    Session::new(store).sign_in(name, password).is_ok()
}

#[test]
fn a_new_password_works_the_old_one_stops_and_the_role_is_untouched() {
    let store = store();
    peopled(&store);

    let mut root = signed_in(&store, "root");
    root.run("ALTER USER ada SET PASSWORD 'a different horse entirely';")
        .unwrap();

    assert!(signs_in(&store, "ada", ROTATED), "the new password");
    assert!(!signs_in(&store, "ada", PASSWORD), "the old password");

    // The role is proven by using it rather than by reading it back: an editor
    // may write, and that is the half a password change must not have touched.
    let mut ada = signed_in_with(&store, "ada", ROTATED);
    ada.run("CREATE orders:1 = { total: 5 };")
        .expect("ada still edits");
}

#[test]
fn a_role_change_turns_a_refusal_into_an_answer_and_the_password_survives() {
    let store = store();
    peopled(&store);

    let mut vic = signed_in(&store, "vic");
    let refused = vic
        .run("CREATE orders:1 = { total: 5 };")
        .expect_err("a viewer may not write");
    assert!(refused.to_string().contains("write"), "{refused}");

    let mut root = signed_in(&store, "root");
    root.run("ALTER USER vic SET ROLE editor;").unwrap();

    // The same password, and now the statement that was refused.
    let mut vic = signed_in(&store, "vic");
    vic.run("CREATE orders:1 = { total: 5 };")
        .expect("vic edits now");
    assert!(!signs_in(&store, "vic", ROTATED), "no other password works");
}

#[test]
fn a_database_owner_cannot_reach_the_store_owner() {
    // The escalation this statement would otherwise be. `nina` passes the
    // `Administer` check — she really does own something — and the containment
    // check is the only thing between her and the node.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    let refused = nina
        .run("ALTER USER root SET PASSWORD 'a different horse entirely';")
        .expect_err("a database owner does not administer the store owner");
    assert!(
        refused.to_string().contains("not in a tenancy"),
        "{refused}"
    );

    // The refusal is the claim; that root is untouched is the thing that
    // matters, and a refusal returned after a write would still be a breach.
    assert!(signs_in(&store, "root", PASSWORD), "root is unchanged");
    assert!(!signs_in(&store, "root", ROTATED), "and not rotated");

    let promoting = nina
        .run("ALTER USER root SET ROLE viewer;")
        .expect_err("nor may she demote them");
    assert!(
        promoting.to_string().contains("not in a tenancy"),
        "{promoting}"
    );
}

#[test]
fn a_database_owner_may_administer_their_own_people() {
    // The other side of the same boundary: refusing everything would be safe
    // and useless, so the test that it *permits* is as load-bearing as the one
    // that it refuses.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    nina.run("ALTER USER ada SET PASSWORD 'a different horse entirely';")
        .unwrap();
    assert!(signs_in(&store, "ada", ROTATED), "nina rotated her editor");

    nina.run("ALTER USER vic SET ROLE editor;").unwrap();
    let mut vic = signed_in(&store, "vic");
    vic.run("CREATE orders:1 = { total: 5 };")
        .expect("vic edits now");
}

#[test]
fn an_editor_may_not_alter_anybody_including_themselves() {
    // Changing your own password without an owner is a reasonable thing for a
    // database to offer and this one does not offer it yet: `ALTER USER` needs
    // `Administer`, and that applies to the caller's own row too.
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("ALTER USER ada SET PASSWORD 'a different horse entirely';")
        .expect_err("an editor may not administer");
    assert!(refused.to_string().contains("administer"), "{refused}");
    assert!(signs_in(&store, "ada", PASSWORD), "ada is unchanged");
}

#[test]
fn altering_a_user_who_is_not_there_says_which_name() {
    let store = store();
    peopled(&store);

    let mut root = signed_in(&store, "root");
    let refused = root
        .run("ALTER USER nobody SET ROLE editor;")
        .expect_err("there is no such user");
    assert!(refused.to_string().contains("nobody"), "{refused}");
}

#[test]
fn a_role_this_build_does_not_have_is_refused_and_changes_nothing() {
    let store = store();
    peopled(&store);

    let mut root = signed_in(&store, "root");
    let refused = root
        .run("ALTER USER vic SET ROLE admin;")
        .expect_err("there is no role called admin");
    assert!(refused.to_string().contains("admin"), "{refused}");

    // Still a viewer, proven by the refusal a viewer gets.
    let mut vic = signed_in(&store, "vic");
    vic.run("CREATE orders:1 = { total: 5 };")
        .expect_err("vic is still a viewer");
}

#[test]
fn looking_a_user_up_stops_at_the_same_boundary_the_listing_does() {
    // The listing filtered and the singular did not, which meant a name nobody
    // would show you was a name you could still read every grant of.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    let refused = nina
        .run("INFO FOR USER root;")
        .expect_err("root is outside nina's tenancy");
    assert!(
        refused.to_string().contains("not in a tenancy"),
        "{refused}"
    );

    nina.run("INFO FOR USER ada;").expect("her own editor");
}
