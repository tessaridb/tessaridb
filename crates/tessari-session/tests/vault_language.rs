//! `DEFINE VAULT` and the words that go with it, through the language.
//!
//! # What these tests are for, and what `sealing.rs` in the storage crate covers
//!
//! The storage tests assert against **bytes**: that a planted plaintext is
//! nowhere in the backend, that a ciphertext moved between records does not
//! open. They cover the mechanism. These cover the **surface** — that the words
//! parse, that they mean what the design says they mean, and that the four
//! refusals refuse.
//!
//! The two halves need each other. A surface test alone would pass against an
//! implementation that stored everything in the clear and merely printed the
//! right words; a byte test alone would pass against an implementation nobody
//! can reach.
//!
//! # The field is called `token` and not `password`, and that is a finding
//!
//! `PASSWORD` is a reserved word — `DEFINE USER ada PASSWORD '…'` — so `DEFINE
//! FIELD password ON team TYPE string SECRET` does not parse, while `CREATE
//! team:'github' SET password = '…'` does, because an object literal's field name
//! accepts a keyword and a declaration's does not. A vault is the one place
//! somebody will certainly try to declare a field called `password`, and the
//! design document's own example does exactly that. Recorded as **Q-417**; the
//! tests use `token` so they test the vault rather than the lexer.
//!
//! # The refusals, and which of them protect anything
//!
//! Only one does. `DEFINE INDEX` over a secret field is a real control: an index
//! is a searchable copy of the plaintext, and a caller who cannot read the field
//! could still ask, a term at a time, whether any record holds a given value.
//!
//! `SELECT` over a vault protects nothing, because what it would return is
//! ciphertext — the sealed envelope *is* the stored value. It is refused so the
//! language means something: opaque bytes teach a caller that the vault is
//! broken, and a refusal naming `REVEAL` teaches them the feature. The
//! distinction is written here because a later reader deciding whether to relax
//! one of these needs to know which is which.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A session with tenancy, an unsealed store and a vault holding one secret.
fn holding(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{TENANCY}
             UNSEAL VAULT WITH 'an operator passphrase';
             DEFINE VAULT team;
             DEFINE FIELD login    ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;"
        ))
        .unwrap();
    session
}

fn refusal(session: &mut Session<'_>, statement: &str) -> String {
    session
        .run(statement)
        .expect_err("the statement was accepted")
        .to_string()
}

fn value(session: &mut Session<'_>, statement: &str) -> Value {
    match session.run(statement).unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

fn write_one(session: &mut Session<'_>) {
    session
        .run(&format!(
            "CREATE team:'github' = {{ login: 'boog', token: '{PLANTED}' }};"
        ))
        .unwrap();
}

#[test]
fn a_vault_is_declared_written_and_revealed() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let opened = value(&mut session, "REVEAL token FROM team:'github';");
    let Value::Object(fields) = opened else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
    // One field asked for, one field answered. A `REVEAL` that quietly returned
    // the whole record would be a `SELECT` that had learned a new word.
    assert_eq!(fields.len(), 1);
}

#[test]
fn a_star_reveals_every_secret_and_no_ordinary_field() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let Value::Object(fields) = value(&mut session, "REVEAL * FROM team:'github';") else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
    // `login` is declared and is not secret. `*` means every *secret* field,
    // because a verb that answers with plaintext must not mix in values that
    // were never sealed — a caller reading the answer cannot tell them apart.
    assert!(!fields.contains_key("login"), "{fields:?}");
}

#[test]
fn select_over_a_vault_is_refused_and_names_reveal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    for statement in [
        "SELECT * FROM team;",
        "SELECT * FROM team WHERE login = 'boog';",
        "SELECT * FROM team:'github';",
        // The ordering and the filter the design names separately. Both are
        // refused here by being reads of a vault at all, which is the stronger
        // form of the same refusal: there is no `SELECT` shape over a vault for
        // an `ORDER BY` to hang off.
        "SELECT * FROM team ORDER BY token;",
        "SELECT * FROM team WHERE token = 'guess';",
    ] {
        let said = refusal(&mut session, statement);
        assert!(said.contains("REVEAL"), "{statement}: {said}");
        assert!(
            !said.contains(PLANTED),
            "{statement} quoted a secret: {said}"
        );
    }
}

#[test]
fn an_index_over_a_secret_field_is_refused() {
    let store = store();
    let mut session = holding(&store);

    let said = refusal(&mut session, "DEFINE INDEX by_token ON team FIELDS token;");
    assert!(said.contains("cannot be indexed"), "{said}");

    // A projection of a secret is a secret somebody derived, so the root of the
    // path is what decides rather than the whole path.
    let said = refusal(
        &mut session,
        "DEFINE INDEX by_part ON team FIELDS token.inner;",
    );
    assert!(said.contains("cannot be indexed"), "{said}");

    // The control: an index over a field that is not secret is fine, so the
    // refusal above is about the marker and not about vaults refusing indexes.
    session
        .run("DEFINE INDEX by_login ON team FIELDS login;")
        .unwrap();
}

#[test]
fn secret_needs_a_vault() {
    let store = store();
    let mut session = holding(&store);
    session.run("DEFINE TABLE staff SCHEMALESS;").unwrap();

    let said = refusal(
        &mut session,
        "DEFINE FIELD token ON staff TYPE string SECRET;",
    );
    assert!(said.contains("needs a vault"), "{said}");
}

#[test]
fn secret_is_refused_in_a_columnar_table_and_in_an_alter() {
    let store = store();
    let mut session = holding(&store);
    session.run("DEFINE TABLE staff SCHEMALESS;").unwrap();

    // Both are grammar refusals rather than catalog ones, and both matter for
    // the same reason: a `SECRET` that parsed and was dropped would leave a
    // field the author believes is sealed.
    let said = refusal(&mut session, "DEFINE TABLE payroll (pay string SECRET);");
    assert!(said.contains("SECRET"), "{said}");

    let said = refusal(
        &mut session,
        "ALTER TABLE team ALTER FIELD token TYPE string SECRET;",
    );
    assert!(said.contains("SECRET"), "{said}");
}

#[test]
fn revealing_a_field_that_is_not_secret_is_refused() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let said = refusal(&mut session, "REVEAL login FROM team:'github';");
    assert!(said.contains("not a secret field"), "{said}");
    assert!(
        !said.contains("boog"),
        "the refusal quoted the value: {said}"
    );
}

#[test]
fn a_sealed_store_refuses_the_write_and_the_reveal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("SEAL VAULT;").unwrap();

    let said = refusal(
        &mut session,
        &format!("CREATE team:'second' = {{ token: '{PLANTED}' }};"),
    );
    assert!(
        !said.contains(PLANTED),
        "the refusal quoted the value: {said}"
    );

    let said = refusal(&mut session, "REVEAL token FROM team:'github';");
    assert!(!said.contains(PLANTED), "{said}");

    // And unsealing again restores it, so sealing is a state rather than a
    // one-way door.
    session
        .run("UNSEAL VAULT WITH 'an operator passphrase';")
        .unwrap();
    let Value::Object(fields) = value(&mut session, "REVEAL token FROM team:'github';") else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
}

#[test]
fn a_wrong_passphrase_is_refused_and_does_not_unseal() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("SEAL VAULT;").unwrap();

    let said = refusal(&mut session, "UNSEAL VAULT WITH 'not the passphrase';");
    assert!(
        !said.contains("not the passphrase"),
        "the refusal quoted the passphrase: {said}"
    );
    // Still sealed. A failed unseal that left the keyring half-open would be
    // worse than one that refused loudly.
    refusal(&mut session, "REVEAL token FROM team:'github';");
}

#[test]
fn the_first_unseal_says_it_initialised_and_the_second_says_it_unsealed() {
    let store = store();
    let mut session = Session::new(&store);
    session.run(TENANCY).unwrap();

    // The distinction that makes a mistyped first passphrase survivable: there
    // is no path that replaces a root once written, so an operator who expected
    // `unsealed` and reads `initialised` has to be told at once.
    assert_eq!(
        value(&mut session, "UNSEAL VAULT WITH 'first';"),
        Value::from("initialised")
    );
    session.run("SEAL VAULT;").unwrap();
    assert_eq!(
        value(&mut session, "UNSEAL VAULT WITH 'first';"),
        Value::from("unsealed")
    );
}

#[test]
fn info_for_vault_reports_which_fields_are_sealed_and_nothing_about_them() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);

    let Value::Object(report) = value(&mut session, "INFO FOR VAULT team;") else {
        panic!("INFO answers with an object")
    };
    let Some(Value::Object(fields)) = report.get("fields") else {
        panic!("the report carries its fields")
    };
    let Some(Value::Object(token)) = fields.get("token") else {
        panic!("the secret field is reported")
    };
    assert_eq!(token.get("secret"), Some(&Value::Bool(true)));

    let Some(Value::Object(login)) = fields.get("login") else {
        panic!("the ordinary field is reported")
    };
    assert_eq!(login.get("secret"), Some(&Value::Bool(false)));

    // No length, no fingerprint, no key identifier — each of which would be an
    // oracle that answers more slowly rather than not at all.
    let rendered = format!("{report:?}");
    assert!(!rendered.contains(PLANTED), "{rendered}");
    assert!(!rendered.to_lowercase().contains("key_id"), "{rendered}");
    assert!(!rendered.to_lowercase().contains("length"), "{rendered}");
}

#[test]
fn dropping_a_vault_makes_its_records_unopenable() {
    let store = store();
    let mut session = holding(&store);
    write_one(&mut session);
    session.run("DROP VAULT team;").unwrap();

    // The vault is gone as a name, which is what a drop of anything does. What
    // makes this one a crypto-shred is that the key went with it: re-declaring
    // the name mints a **new** key, so nothing written under the old one is
    // reachable from here or from any restored copy.
    let said = refusal(&mut session, "REVEAL token FROM team:'github';");
    assert!(!said.contains(PLANTED), "{said}");

    session.run("DEFINE VAULT team;").unwrap();
    session
        .run("DEFINE FIELD token ON team TYPE string SECRET;")
        .unwrap();
    // Absent, not opened, and `none` rather than an empty object: the record
    // went with the table, and the re-declared vault holds a **new** key, so
    // there is nothing here that could open the old bytes even if a restore put
    // them back. That is the crypto-shred, stated as the only thing a test in
    // this process can actually observe about it.
    assert_eq!(
        value(&mut session, "REVEAL token FROM team:'github';"),
        Value::None
    );
}

#[test]
fn define_vault_needs_an_unsealed_store() {
    let store = store();
    let mut session = Session::new(&store);
    session.run(TENANCY).unwrap();

    // Before any `UNSEAL` there is no master key, so there is nothing to wrap
    // the vault's own key under. Refused rather than deferred: a vault created
    // with the key left for later would refuse every write while `INFO`
    // reported it ready.
    refusal(&mut session, "DEFINE VAULT team;");
}
