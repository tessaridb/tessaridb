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
             DEFINE COLLECTION orders;",
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
    // database to offer and this one does not offer it yet: `ALTER USER`
    // demands `govern`, and that applies to the caller's own row too.
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("ALTER USER ada SET PASSWORD 'a different horse entirely';")
        .expect_err("an editor may not govern");
    assert!(refused.to_string().contains("govern"), "{refused}");
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

#[test]
fn a_database_owner_cannot_drop_the_store_owner() {
    // The costliest instance of the same boundary, and the one that was missing
    // longest. A closed store has no back door, so removing the store's owner
    // is not "exceeding your authority and being caught" — it is permanent. The
    // account is gone, its grants are gone, and nothing can put either back.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    let refused = nina
        .run("DROP USER root;")
        .expect_err("a database owner does not administer the store owner");
    assert!(
        refused.to_string().contains("not in a tenancy"),
        "{refused}"
    );

    // The refusal is not the claim. Still being able to sign in is.
    assert!(signs_in(&store, "root", PASSWORD), "root is still there");
}

#[test]
fn a_database_owner_may_drop_their_own_people() {
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    nina.run("DROP USER vic;").unwrap();
    assert!(!signs_in(&store, "vic", PASSWORD), "vic is gone");
    // And the ones she may not touch are untouched by the same statement's
    // neighbours running fine.
    assert!(signs_in(&store, "ada", PASSWORD), "ada is not");
}

#[test]
fn dropping_somebody_who_is_not_there_is_still_not_an_error() {
    // The pre-existing contract, which the new check must not have changed: the
    // statement asks for a store without that name, and it already is one. The
    // check fires on a user that EXISTS and is out of reach, which is a
    // different question from one that does not exist at all.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    nina.run("DROP USER nobody;")
        .expect("no such user, no error");
}

#[test]
fn an_editor_may_not_drop_anybody() {
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("DROP USER vic;")
        .expect_err("an editor may not govern");
    assert!(refused.to_string().contains("govern"), "{refused}");
    assert!(signs_in(&store, "vic", PASSWORD), "vic is still there");
}

#[test]
fn a_database_owner_cannot_declare_somebody_wider_than_themselves() {
    // The hole that made the other four checks decorative. `nina` may not touch
    // `root`, but if she may *declare* an owner with no tenancy she simply signs
    // in as them and does whatever she likes. Bounding the statements that
    // change an existing user, without bounding the one that creates one, closes
    // every door in a room with no walls.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    let refused = nina
        .run("DEFINE USER mallory ROLE owner PASSWORD 'correct horse battery';")
        .expect_err("a database owner may not declare a node administrator");
    assert!(
        refused.to_string().contains("further than you"),
        "{refused}"
    );
    assert!(!signs_in(&store, "mallory", PASSWORD), "and none was made");

    // Inside her own tenancy she may, which is the half that must keep working.
    nina.run("DEFINE USER junior ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .expect("her own space is hers");
    assert!(signs_in(&store, "junior", PASSWORD), "junior exists");
}

#[test]
fn a_grant_cannot_be_aimed_at_somebody_you_do_not_administer() {
    // A grant reads as generosity and acts as a narrowing: the store's rule is
    // that a user with even one grant is reduced to exactly what they were
    // granted. So aiming one upwards is how an owner of a part takes authority
    // away from the owner of the whole — and `BACKUP`, the store's only recovery
    // path, is the first thing to go.
    let store = store();
    peopled(&store);

    let mut nina = signed_in(&store, "nina");
    let refused = nina
        .run("GRANT read ON orders TO root;")
        .expect_err("root is not in nina's tenancy");
    assert!(
        refused.to_string().contains("not in a tenancy"),
        "{refused}"
    );

    // The proof is not the refusal. It is that root's authority is intact.
    let mut root = signed_in(&store, "root");
    root.run("BACKUP;")
        .expect("root can still back the store up");

    // Downwards, within her own tenancy, a grant still works.
    nina.run("GRANT read ON orders TO ada;")
        .expect("ada is hers to narrow");
}

#[test]
fn the_first_user_of_an_empty_store_may_still_be_a_store_wide_owner() {
    // The one moment the reach check must NOT fire. Nobody is signed in, the
    // store is open, and if an anonymous session could not declare an owner with
    // no tenancy then no store could ever be closed at all.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run("DEFINE NAMESPACE prod; DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .expect("the first owner closes the store");
    assert!(signs_in(&store, "root", PASSWORD), "root exists");
}
