//! `RESTORE SCRIPT FROM '<name>'` — a script backup run into a live store.
//!
//! The claims: a script restores beside what the store already holds, reusing a
//! namespace that exists and creating only databases that do not; a database
//! that already exists refuses the whole restore and nothing is written; a
//! script that does anything but create new places and fill them — a delete, a
//! user, a write into a place it did not create — is refused before anything is
//! written; the file is read only from inside the backup folder; and only a
//! store-wide owner may run it.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;

const PASSWORD: &str = "a long one";

fn store(script: &str) -> Store {
    let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
    let store = Store::open(backend).unwrap();
    Session::new(&store)
        .run(&format!(
            "{script}
             BEGIN;
             DEFINE USER root ROLE owner PASSWORD 'a long one';
             DEFINE USER nina ON NAMESPACE prod ROLE owner PASSWORD 'a long one';
             COMMIT;"
        ))
        .unwrap();
    store
}

/// The store a backup is taken of.
fn source() -> Store {
    store(
        "DEFINE ANALYZER words FILTERS lowercase;
         DEFINE NAMESPACE prod; USE NAMESPACE prod;
         DEFINE DATABASE orders; USE DATABASE orders;
         DEFINE TABLE items SCHEMALESS;
         DEFINE FIELD note ON items TYPE string ANALYZER words;
         CREATE items:1 = { note: 'Kept' }; CREATE items:2 = { note: 'Also' };
         DEFINE NAMESPACE crm; USE NAMESPACE crm;
         DEFINE DATABASE people; USE DATABASE people;
         DEFINE COLLECTION contacts; CREATE contacts:1 = { n: 'x' };",
    )
}

/// The live store a backup is restored into: its own data, including a
/// namespace `prod` that holds a different database.
fn target() -> Store {
    store(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;
         DEFINE DATABASE billing; USE DATABASE billing;
         DEFINE COLLECTION invoices; CREATE invoices:1 = { n: 1 };
         DEFINE NAMESPACE other; USE NAMESPACE other;
         DEFINE DATABASE x; USE DATABASE x;
         DEFINE COLLECTION y; CREATE y:1 = { n: 2 };",
    )
}

fn signed_in<'a>(store: &'a Store, name: &str, folder: &Path) -> Session<'a> {
    let mut session = Session::new(store).backing_up_into(Arc::from(folder));
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// How many records a read answers, or `None` when the place is refused.
fn held(session: &mut Session<'_>, place: &str, table: &str) -> Option<usize> {
    session.run(place).ok()?;
    match session.run(&format!("SELECT * FROM {table};")).ok()?.pop() {
        Some(Outcome::Records { records, .. }) => Some(records.len()),
        other => panic!("SELECT answered {other:?}"),
    }
}

fn refused(session: &mut Session<'_>, statement: &str) -> Error {
    match session.run(statement) {
        Ok(answered) => panic!("{statement} was not refused: {answered:?}"),
        Err(error) => error,
    }
}

#[test]
fn a_part_restores_beside_what_the_store_holds() {
    let folder = tempfile::tempdir().unwrap();
    let from = source();
    signed_in(&from, "root", folder.path())
        .run("BACKUP SCRIPT OF prod.orders, NAMESPACE crm TO 'part.tessariql';")
        .unwrap();

    let into = target();
    let mut root = signed_in(&into, "root", folder.path());
    root.run("RESTORE SCRIPT FROM 'part.tessariql';").unwrap();

    // Restored: a database beside an existing one in `prod`, and a new namespace.
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE prod; USE DATABASE orders;",
            "items"
        ),
        Some(2)
    );
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE crm; USE DATABASE people;",
            "contacts"
        ),
        Some(1)
    );
    // Untouched.
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE prod; USE DATABASE billing;",
            "invoices"
        ),
        Some(1)
    );
    assert_eq!(
        held(&mut root, "USE NAMESPACE other; USE DATABASE x;", "y"),
        Some(1)
    );
    // The search the analyzer serves came across with the field.
    root.run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE prod; USE DATABASE orders;",
            "items WHERE note MATCHES 'kept'"
        ),
        Some(1)
    );
}

#[test]
fn a_database_that_exists_refuses_the_whole_restore() {
    let folder = tempfile::tempdir().unwrap();
    let from = source();
    signed_in(&from, "root", folder.path())
        .run("BACKUP SCRIPT OF prod.orders, NAMESPACE crm TO 'part.tessariql';")
        .unwrap();

    let into = target();
    let mut root = signed_in(&into, "root", folder.path());
    root.run("USE NAMESPACE prod; DEFINE DATABASE orders;")
        .unwrap();

    let error = refused(&mut root, "RESTORE SCRIPT FROM 'part.tessariql';");
    assert!(
        matches!(error, Error::RestoreTargetExists { .. }),
        "refused as {error:?}"
    );
    // Nothing of it was written — not even the namespace that did not exist.
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE crm; USE DATABASE people;",
            "contacts"
        ),
        None
    );
}

#[test]
fn a_script_that_does_more_than_create_and_fill_is_refused_before_anything_is_written() {
    let folder = tempfile::tempdir().unwrap();
    let into = target();
    let mut root = signed_in(&into, "root", folder.path());

    for (file, script) in [
        (
            "writes-elsewhere.tessariql",
            "DEFINE NAMESPACE fresh; USE NAMESPACE fresh; DEFINE DATABASE d; USE DATABASE d;
             DEFINE COLLECTION c; CREATE c:1 = {};
             USE NAMESPACE other; USE DATABASE x; CREATE y:9 = { n: 9 };",
        ),
        (
            "deletes.tessariql",
            "DEFINE NAMESPACE fresh; USE NAMESPACE fresh; DEFINE DATABASE d; USE DATABASE d;
             DEFINE COLLECTION c; DELETE c:1;",
        ),
        (
            "whole-store.tessariql",
            "DEFINE NAMESPACE fresh; USE NAMESPACE fresh; DEFINE DATABASE d; USE DATABASE d;
             DEFINE COLLECTION c; BEGIN; DEFINE USER eve ROLE owner PASSWORD 'a long one'; COMMIT;",
        ),
    ] {
        std::fs::write(folder.path().join(file), script).unwrap();
        let error = refused(&mut root, &format!("RESTORE SCRIPT FROM '{file}';"));
        assert!(
            matches!(error, Error::RestoreRefused { .. }),
            "{file} was refused as {error:?}"
        );
        assert_eq!(
            held(&mut root, "USE NAMESPACE fresh; USE DATABASE d;", "c"),
            None,
            "{file} wrote before it was refused"
        );
    }
    assert_eq!(
        held(&mut root, "USE NAMESPACE other; USE DATABASE x;", "y"),
        Some(1)
    );
}

#[test]
fn the_file_is_read_only_from_inside_the_folder_and_only_by_an_owner() {
    let outside = tempfile::tempdir().unwrap();
    let folder = outside.path().join("backups");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(
        outside.path().join("escaped.tessariql"),
        "DEFINE NAMESPACE fresh;",
    )
    .unwrap();
    std::fs::write(folder.join("fine.tessariql"), "DEFINE NAMESPACE fresh;").unwrap();
    let into = target();

    let mut root = signed_in(&into, "root", &folder);
    for name in ["../escaped.tessariql", "missing.tessariql"] {
        let error = refused(&mut root, &format!("RESTORE SCRIPT FROM '{name}';"));
        assert!(
            matches!(error, Error::BackupNameRefused { .. }),
            "{name} was refused as {error:?}"
        );
    }

    let mut nina = signed_in(&into, "nina", &folder);
    let error = refused(&mut nina, "RESTORE SCRIPT FROM 'fine.tessariql';");
    assert!(
        !matches!(
            error,
            Error::BackupNameRefused { .. }
                | Error::RestoreRefused { .. }
                | Error::RestoreTargetExists { .. }
        ),
        "an owner of one database reached the restore itself: {error:?}"
    );
    assert!(
        root.run("USE NAMESPACE fresh;").is_err(),
        "a refused restore wrote"
    );
}

#[test]
fn a_restore_that_fails_while_filling_leaves_nothing_behind() {
    let folder = tempfile::tempdir().unwrap();
    let into = target();
    let mut root = signed_in(&into, "root", folder.path());
    // Vetted, and then refused by the store while it runs: the record is written
    // twice. The namespace and database the first steps created are taken away.
    std::fs::write(
        folder.path().join("fails-late.tessariql"),
        "DEFINE NAMESPACE fresh; USE NAMESPACE fresh; DEFINE DATABASE d; USE DATABASE d;
         DEFINE COLLECTION c; CREATE c:1 = {}; CREATE c:1 = {};
         DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE extra; USE DATABASE extra;
         DEFINE COLLECTION e;",
    )
    .unwrap();

    let error = refused(&mut root, "RESTORE SCRIPT FROM 'fails-late.tessariql';");

    assert!(
        matches!(error, Error::RecordExists { .. }),
        "refused as {error:?}"
    );
    assert!(
        root.run("USE NAMESPACE fresh;").is_err(),
        "the namespace it created is still there"
    );
    assert!(
        root.run("USE NAMESPACE prod; USE DATABASE extra;").is_err(),
        "the database it created in an existing namespace is still there"
    );
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE prod; USE DATABASE billing;",
            "invoices"
        ),
        Some(1)
    );
}

fn at_rest(byte: u8) -> Arc<tessari_vault::AtRestKey> {
    Arc::new(
        tessari_vault::AtRestKey::from_key(&tessari_vault::SecretBytes::adopt([byte; 32])).unwrap(),
    )
}

/// A sealed script restores on a node holding the key it was sealed under, and
/// is refused — naming why — on a node with no key or another one, with
/// nothing written (ADR-0108 D7).
#[test]
fn a_sealed_script_restores_only_under_its_own_key() {
    let folder = tempfile::tempdir().unwrap();
    let from = source();
    signed_in(&from, "root", folder.path())
        .sealing_backups(at_rest(7))
        .run("BACKUP SCRIPT OF prod.orders TO 'sealed.tessariql';")
        .unwrap();

    let into = target();
    let unkeyed = refused(
        &mut signed_in(&into, "root", folder.path()),
        "RESTORE SCRIPT FROM 'sealed.tessariql';",
    )
    .to_string();
    assert!(unkeyed.contains("given its encryption key"), "{unkeyed}");
    let other = refused(
        &mut signed_in(&into, "root", folder.path()).sealing_backups(at_rest(8)),
        "RESTORE SCRIPT FROM 'sealed.tessariql';",
    )
    .to_string();
    assert!(other.contains("does not open under this key"), "{other}");
    let mut root = signed_in(&into, "root", folder.path());
    assert_eq!(
        held(
            &mut root,
            "USE NAMESPACE prod; USE DATABASE orders;",
            "items"
        ),
        None,
        "a refused restore wrote something"
    );

    let mut keyed = signed_in(&into, "root", folder.path()).sealing_backups(at_rest(7));
    keyed
        .run("RESTORE SCRIPT FROM 'sealed.tessariql';")
        .unwrap();
    assert_eq!(
        held(
            &mut keyed,
            "USE NAMESPACE prod; USE DATABASE orders;",
            "items"
        ),
        Some(2)
    );
}
