//! A user re-created from the hash the store holds (ADR-0091).
//!
//! A state script can never know a password — the store never kept one — so it
//! writes the user back with `PASSHASH` and the stored Argon2id string. What has
//! to hold: the user signs in with the password they always had, and the form is
//! no way to plant a credential weaker than the store would have made itself.

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::{Catalog, Store};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// The stored hash of the user named `name`.
fn stored(store: &Store, name: &str) -> String {
    let mut view = store.begin().unwrap();
    Catalog::new(&mut view)
        .users()
        .unwrap()
        .into_iter()
        .find(|user| user.name == name)
        .map(|user| user.secret)
        .unwrap()
}

#[test]
fn a_user_defined_by_their_stored_hash_signs_in_with_their_password() {
    let source = store();
    Session::new(&source)
        .run(&format!(
            "DEFINE USER ada ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let hash = stored(&source, "ada");

    let target = store();
    Session::new(&target)
        .run(&format!("DEFINE USER ada ROLE owner PASSHASH '{hash}';"))
        .unwrap();
    assert_eq!(
        stored(&target, "ada"),
        hash,
        "the hash was not stored as given"
    );
    assert!(Session::new(&target).sign_in("ada", PASSWORD).is_ok());
    assert!(Session::new(&target).sign_in("ada", "not it").is_err());
}

/// A real Argon2id hash of `PASSWORD` at these parameters.
fn argon2id(memory: u32, passes: u32) -> String {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let salt = SaltString::encode_b64(b"a fixed salt!!!!").unwrap();
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(memory, passes, 1, None).unwrap(),
    )
    .hash_password(PASSWORD.as_bytes(), &salt)
    .unwrap()
    .to_string()
}

#[test]
fn a_hash_this_store_would_not_have_made_is_refused() {
    // Each weak case differs from an accepted one in ONE parameter, so each floor
    // is what refuses it — a case weak in two ways would pass with either check
    // missing.
    let floor = argon2id(
        tessari_constants::PASSWORD_HASH_MEMORY_KIB,
        tessari_constants::PASSWORD_HASH_PASSES,
    );
    assert!(
        Session::new(&store())
            .run(&format!("DEFINE USER ada ROLE owner PASSHASH '{floor}';"))
            .is_ok(),
        "a hash at the store's own parameters was refused"
    );
    for weak in [
        argon2id(4096, tessari_constants::PASSWORD_HASH_PASSES),
        argon2id(tessari_constants::PASSWORD_HASH_MEMORY_KIB, 1),
        "$2b$12$R9h/cIPz0gi.URNNX3kh2OPST9/PgBkqquzi.Ss7KIUgO2t0jWMUW".to_owned(),
        "hunter2".to_owned(),
    ] {
        let refused = Session::new(&store())
            .run(&format!("DEFINE USER ada ROLE owner PASSHASH '{weak}';"))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("not one this store would store"),
            "{weak}: {refused}"
        );
    }
}
