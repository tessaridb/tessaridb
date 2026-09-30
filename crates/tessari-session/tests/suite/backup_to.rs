//! `BACKUP … TO '<name>'` — a backup written into the node's backup folder.
//!
//! The claims: each form lands under its name with the bytes the same statement
//! without `TO` answers; a name that would leave the folder — by `..`, by being
//! absolute, or through a symlinked subfolder — is refused and writes nothing;
//! an existing file is never overwritten; a node with no folder refuses; and
//! only a store-wide owner may run it at all.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

const PASSWORD: &str = "a long one";

fn store() -> Store {
    let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
    let store = Store::open(backend).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE app; USE DATABASE app;
             DEFINE COLLECTION notes; CREATE notes:1 = { body: 'kept' };
             BEGIN;
             DEFINE USER root ROLE owner PASSWORD 'a long one';
             DEFINE USER nina ON prod.app ROLE owner PASSWORD 'a long one';
             COMMIT;",
        )
        .unwrap();
    store
}

fn owner<'a>(store: &'a Store, folder: &Path) -> Session<'a> {
    let mut session = Session::new(store).backing_up_into(Arc::from(folder));
    session.sign_in("root", PASSWORD).unwrap();
    session
}

/// The one value a one-statement script answered.
fn answer(session: &mut Session<'_>, script: &str) -> Value {
    match session.run(script).unwrap().pop() {
        Some(Outcome::Value(value)) => value,
        other => panic!("{script} answered {other:?}"),
    }
}

fn field<'v>(answer: &'v Value, name: &str) -> &'v Value {
    let Value::Object(fields) = answer else {
        panic!("not an object: {answer:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no {name} in {answer:?}"))
}

fn refused(session: &mut Session<'_>, script: &str) -> Error {
    match session.run(script) {
        Ok(answered) => panic!("{script} was not refused: {answered:?}"),
        Err(error) => error,
    }
}

#[test]
fn each_form_lands_in_the_folder_with_the_bytes_the_statement_answers() {
    let store = store();
    let folder = tempfile::tempdir().unwrap();
    let mut root = owner(&store, folder.path());

    for (form, statement, name) in [
        ("log", "BACKUP", "nightly.tessarilog"),
        ("state", "BACKUP STATE", "nightly.tessarisnap"),
        ("script", "BACKUP SCRIPT", "nightly.tessariql"),
    ] {
        let written = answer(&mut root, &format!("{statement} TO '{name}';"));
        let landed = folder.path().join(name);
        let held = std::fs::read(&landed).unwrap();

        let expected = match answer(&mut root, &format!("{statement};")) {
            Value::Bytes(bytes) => bytes,
            Value::String(text) => text.into_bytes(),
            other => panic!("{statement} answered {other:?}"),
        };
        assert!(!held.is_empty(), "{name} is empty");
        assert_eq!(held, expected, "{name} is not what `{statement};` answers");
        assert_eq!(field(&written, "form"), &Value::String(form.to_owned()));
        assert_eq!(
            field(&written, "bytes"),
            &Value::Number(Number::Integer(i64::try_from(held.len()).unwrap()))
        );
        assert_eq!(
            field(&written, "path"),
            &Value::String(
                landed
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }
    // Nothing is left beside them: the partial file is renamed, not copied.
    let mut names: Vec<String> = std::fs::read_dir(folder.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "nightly.tessarilog",
            "nightly.tessariql",
            "nightly.tessarisnap"
        ]
    );
}

#[test]
fn a_subfolder_named_in_the_file_is_made_inside_the_folder() {
    let store = store();
    let folder = tempfile::tempdir().unwrap();
    let mut root = owner(&store, folder.path());

    answer(
        &mut root,
        "BACKUP STATE TO 'weekly/2026/state.tessarisnap';",
    );

    assert!(
        folder
            .path()
            .join("weekly/2026/state.tessarisnap")
            .is_file()
    );
}

#[test]
fn a_name_that_would_leave_the_folder_is_refused_and_writes_nothing() {
    let store = store();
    let outside = tempfile::tempdir().unwrap();
    let folder = outside.path().join("backups");
    std::fs::create_dir(&folder).unwrap();
    let mut root = owner(&store, &folder);

    let escape = outside.path().join("escaped.tessarilog");
    for name in [
        "../escaped.tessarilog",
        "weekly/../../escaped.tessarilog",
        escape.to_str().unwrap(),
        "",
        ".",
        "./x.tessarilog",
        "weekly/",
    ] {
        let error = refused(&mut root, &format!("BACKUP TO '{name}';"));
        assert!(
            matches!(error, Error::BackupNameRefused { .. }),
            "{name:?} was refused as {error:?}, not as a name leaving the folder"
        );
    }
    assert!(!escape.exists(), "a refused backup was written outside");
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn a_symlinked_subfolder_does_not_lead_out_of_the_folder() {
    let store = store();
    let folder = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), folder.path().join("out")).unwrap();
    let mut root = owner(&store, folder.path());

    let error = refused(&mut root, "BACKUP TO 'out/escaped.tessarilog';");

    assert!(
        matches!(error, Error::BackupNameRefused { .. }),
        "refused as {error:?}"
    );
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[test]
fn an_existing_file_is_never_overwritten() {
    let store = store();
    let folder = tempfile::tempdir().unwrap();
    let kept = folder.path().join("nightly.tessarilog");
    std::fs::write(&kept, b"an earlier backup").unwrap();
    let mut root = owner(&store, folder.path());

    let error = refused(&mut root, "BACKUP TO 'nightly.tessarilog';");

    assert!(
        matches!(error, Error::BackupExists { .. }),
        "refused as {error:?}"
    );
    assert_eq!(std::fs::read(&kept).unwrap(), b"an earlier backup");
}

#[test]
fn a_node_with_no_folder_refuses_and_backup_without_to_still_answers() {
    let store = store();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();

    let error = refused(&mut root, "BACKUP STATE TO 'state.tessarisnap';");

    assert!(
        matches!(error, Error::NoBackupFolder),
        "refused as {error:?}"
    );
    assert!(matches!(
        answer(&mut root, "BACKUP STATE;"),
        Value::Bytes(_)
    ));
}

#[test]
fn only_a_store_wide_owner_may_write_one() {
    let store = store();
    let folder = tempfile::tempdir().unwrap();
    let mut nina = Session::new(&store).backing_up_into(Arc::from(folder.path()));
    nina.sign_in("nina", PASSWORD).unwrap();

    let error = refused(&mut nina, "BACKUP TO 'nightly.tessarilog';");

    assert!(
        !matches!(
            error,
            Error::BackupNameRefused { .. } | Error::BackupExists { .. } | Error::NoBackupFolder
        ),
        "an owner of one database reached the file checks: {error:?}"
    );
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0);
}
