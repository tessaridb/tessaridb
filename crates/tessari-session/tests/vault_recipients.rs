//! The recipient set: opaque to the engine, and edited without re-sealing.
//!
//! # What is actually being asserted
//!
//! Two properties, and neither is readable from the code that implements them.
//!
//! **Opacity** is asserted behaviourally rather than by grepping the source for
//! branches. A grep proves what the engine does not *mention*; these tests plant
//! names the engine might plausibly have been written to notice — the reserved
//! field's own spelling, the store's entry in a different case, the empty
//! string, an address, a URN — and assert every one of them is stored and
//! returned exactly as given. A branch nobody wrote a name for cannot hide from
//! that, and a branch somebody wrote a *pattern* for could hide from a grep.
//!
//! **Byte-identity** is asserted against the backend rather than through the
//! language, because through the language there is nothing to see: `SELECT` over
//! a vault is refused and `REVEAL` answers plaintext, so the only place the
//! ciphertext exists as an observable is the raw store. A test that checked
//! `REVEAL` still answered would pass against an implementation that re-sealed
//! every field on every recipient added — which is exactly the defect criterion
//! F2 exists to forbid, and the one that would show up months later as a backup
//! diff nobody could explain.
//!
//! # The refusal that matters most is the one for a name that is not there
//!
//! `REMOVE RECIPIENT` on an absent name is an error and not a no-op. A
//! revocation that matches nothing and answers `ok` leaves the operator
//! believing a party was removed while their entry is still on the record, and
//! there is no later moment at which anything is in an error state.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::decode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Parameters, Session};
use tessari_storage::{Catalog, KEYS_FIELD, RecordAddress, Store, VAULT_RECIPIENT};
use tessari_types::{RecordId, Value};

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
";

/// The same tenancy, selected rather than declared — for every session after
/// the first one, which already declared it.
const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// A store holding one vault record with one sealed field.
fn holding() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "{TENANCY}
                 UNSEAL VAULT WITH 'an operator passphrase';
                 DEFINE VAULT team;
                 DEFINE FIELD login ON team TYPE string;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 CREATE team:'github' = {{ login: 'boog', token: '{PLANTED}' }};"
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

/// The recipient set as `INFO` reports it.
fn listed(session: &mut Session<'_>) -> BTreeMap<String, Value> {
    let Value::Object(report) = value(session, "INFO FOR RECIPIENTS OF team:'github';") else {
        panic!("the report is not an object");
    };
    let Some(Value::Object(entries)) = report.get("recipients") else {
        panic!("the report carries no recipient set");
    };
    entries.clone()
}

/// The vault record as the store currently holds it.
///
/// Read through a transaction rather than by scanning the backend, and that is
/// the second attempt: a scan sees **every MVCC version** of the record, so
/// after one edit it finds two and after two it finds three. The version a test
/// about byte-identity has to compare is the current one, and a transaction is
/// the thing that knows which that is.
fn stored_record(store: &Store) -> BTreeMap<String, Value> {
    let mut transaction = store.begin().unwrap();
    let namespace = Catalog::new(&mut transaction)
        .namespaces()
        .unwrap()
        .into_iter()
        .find(|namespace| namespace.name == "prod")
        .expect("no namespace")
        .id;
    let database = Catalog::new(&mut transaction)
        .databases_in(namespace)
        .unwrap()
        .into_iter()
        .find(|database| database.name == "work")
        .expect("no database")
        .id;
    let table = Catalog::new(&mut transaction)
        .table_id(namespace, database, "team")
        .unwrap()
        .expect("no vault");
    let address = RecordAddress::new(
        namespace,
        database,
        table,
        RecordId::Text("github".to_owned()),
    );
    let stored = transaction.get(&address).unwrap().expect("no record");
    let Value::Object(fields) = decode_payload(&stored).unwrap() else {
        panic!("the record is not an object");
    };
    fields
}

/// The sealed material of a record: every field but the key set, and the store's
/// own entry within it. Everything F2 says an edit must leave untouched.
fn untouchable(fields: &BTreeMap<String, Value>) -> (BTreeMap<String, Value>, Value) {
    let mut rest: BTreeMap<String, Value> = fields.clone();
    let keys = rest.remove(KEYS_FIELD).expect("no key set");
    let Value::Object(entries) = keys else {
        panic!("the key set is not an object");
    };
    let own = entries
        .get(VAULT_RECIPIENT)
        .expect("no entry for the store")
        .clone();
    (rest, own)
}

#[test]
fn two_identifiers_the_engine_cannot_read_round_trip_unchanged() {
    let store = holding();
    let mut session = session_on(&store);

    session
        .run(
            "ADD RECIPIENT 'urn:acme:kms:key/42' TO team:'github' KEY 0xdeadbeef;
             ADD RECIPIENT 'ada@example.com' TO team:'github' KEY 0x0102030405;",
        )
        .unwrap();

    let entries = listed(&mut session);
    assert_eq!(entries.len(), 2, "expected exactly the two that were added");
    assert_eq!(
        entries.get("urn:acme:kms:key/42"),
        Some(&Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
    );
    assert_eq!(
        entries.get("ada@example.com"),
        Some(&Value::Bytes(vec![1, 2, 3, 4, 5])),
    );
}

#[test]
fn a_name_the_engine_might_have_noticed_is_carried_like_any_other() {
    let store = holding();
    let mut session = session_on(&store);

    // Every one of these is a name an implementation could plausibly have been
    // written to treat specially: the reserved field's own spelling, the store's
    // entry in another case, the entry without its marker, the empty string, and
    // text carrying the punctuation a key grammar would care about.
    let awkward = [
        "#keys",
        "#Vault",
        "vault",
        "",
        "a name with spaces",
        "юникод-☃-снаружи-ascii",
        "colon:slash/dot.",
    ];
    for (n, name) in awkward.iter().enumerate() {
        session
            .run(&format!(
                "ADD RECIPIENT '{name}' TO team:'github' KEY 0x{:02x};",
                n + 1
            ))
            .unwrap();
    }

    let entries = listed(&mut session);
    assert_eq!(entries.len(), awkward.len());
    for (n, name) in awkward.iter().enumerate() {
        assert_eq!(
            entries.get(*name),
            Some(&Value::Bytes(vec![u8::try_from(n + 1).unwrap()])),
            "`{name}` did not come back as it went in",
        );
    }
}

#[test]
fn the_stores_own_entry_is_neither_addable_nor_removable() {
    let store = holding();
    let mut session = session_on(&store);

    let added = refusal(
        &mut session,
        "ADD RECIPIENT '#vault' TO team:'github' KEY 0x00;",
    );
    assert!(added.contains("#vault"), "{added}");

    let removed = refusal(
        &mut session,
        "REMOVE RECIPIENT '#vault' FROM team:'github';",
    );
    assert!(removed.contains("#vault"), "{removed}");

    // And the entry is still there afterwards — the record still opens.
    assert!(
        matches!(
            value(&mut session, "REVEAL token FROM team:'github';"),
            Value::Object(_)
        ),
        "the record stopped opening",
    );
}

#[test]
fn adding_a_recipient_leaves_every_ciphertext_byte_identical() {
    let store = holding();
    let before = untouchable(&stored_record(&store));

    let mut session = session_on(&store);
    session
        .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xfeed;")
        .unwrap();

    let after_fields = stored_record(&store);
    let after = untouchable(&after_fields);
    assert_eq!(before.0, after.0, "a field outside the key set changed");
    assert_eq!(before.1, after.1, "the store's own wrapped key changed");

    // And the added entry is the only difference.
    let Some(Value::Object(entries)) = after_fields.get(KEYS_FIELD) else {
        panic!("no key set");
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(entries.get("ops"), Some(&Value::Bytes(vec![0xfe, 0xed])));
}

#[test]
fn removing_the_recipient_restores_the_record_byte_for_byte() {
    let store = holding();
    let before = stored_record(&store);

    let mut session = session_on(&store);
    session
        .run(
            "ADD RECIPIENT 'ops' TO team:'github' KEY 0xfeed;
             REMOVE RECIPIENT 'ops' FROM team:'github';",
        )
        .unwrap();

    assert_eq!(
        before,
        stored_record(&store),
        "a round trip through the recipient set did not leave the record as it was",
    );
}

#[test]
fn a_duplicate_add_is_refused_rather_than_overwriting() {
    let store = holding();
    let mut session = session_on(&store);
    session
        .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xaaaa;")
        .unwrap();

    let refused = refusal(
        &mut session,
        "ADD RECIPIENT 'ops' TO team:'github' KEY 0xbbbb;",
    );
    assert!(refused.contains("already"), "{refused}");

    // The first material survived. Overwriting would have destroyed the only
    // copy of whatever the existing entry held, in one statement and silently.
    assert_eq!(
        listed(&mut session).get("ops"),
        Some(&Value::Bytes(vec![0xaa, 0xaa])),
    );
}

#[test]
fn removing_a_recipient_that_is_not_there_is_refused() {
    let store = holding();
    let mut session = session_on(&store);
    session
        .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xaaaa;")
        .unwrap();

    // A typo in a revocation. Answering `ok` here is the failure this refusal
    // exists to prevent: the operator would close the ticket believing `opz`
    // was removed, while `ops` still holds its entry.
    let refused = refusal(&mut session, "REMOVE RECIPIENT 'opz' FROM team:'github';");
    assert!(refused.contains("opz"), "{refused}");
    assert_eq!(listed(&mut session).len(), 1);
}

#[test]
fn a_recipient_is_removed_while_the_store_is_sealed() {
    let store = holding();
    let mut session = session_on(&store);
    session
        .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xaaaa;")
        .unwrap();
    session.run("SEAL VAULT;").unwrap();

    // Revoking is the operation you least want to need an operator for, and
    // nothing here unwraps a key — so a sealed store can still do it.
    session
        .run("REMOVE RECIPIENT 'ops' FROM team:'github';")
        .unwrap();
    assert!(listed(&mut session).is_empty());

    // The reading of a secret is still refused, which is what says the store is
    // genuinely sealed and this was not a test of an unsealed one.
    let refused = refusal(&mut session, "REVEAL token FROM team:'github';");
    assert!(refused.contains("sealed"), "{refused}");
}

#[test]
fn an_index_on_a_plain_field_still_answers_after_a_recipient_is_added() {
    let store = holding();
    let mut session = session_on(&store);
    // A vault may carry an index on a field that is not secret, and the
    // recipient edit deliberately does not re-index. This is the falsification
    // of that decision: if the write went round the index maintenance in a way
    // that mattered, the record would stop being found.
    session
        .run("DEFINE INDEX by_login ON team FIELDS login;")
        .unwrap();
    session
        .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xfeed;")
        .unwrap();

    // Reached through the vault's own verb, since `SELECT` over a vault is
    // refused: what the index has to survive is the write, and the read that
    // proves the record is still there is `REVEAL`.
    let opened = value(&mut session, "REVEAL token FROM team:'github';");
    let Value::Object(fields) = opened else {
        panic!("expected an object");
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned())),
    );
}

#[test]
fn the_recipient_set_is_not_readable_without_the_grant() {
    let store = holding();
    {
        let mut owner = session_on(&store);
        owner
            .run(
                // The recipient and the second table first: declaring the
                // store's first user closes it to anonymous sessions.
                "ADD RECIPIENT 'ops' TO team:'github' KEY 0xfeed;
                 DEFINE TABLE notes SCHEMALESS;
                 DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
            )
            .unwrap();
        let mut root = Session::new(&store);
        root.sign_in("root", "correct horse battery").unwrap();
        root.run(&format!(
            "{USING}
             DEFINE USER ada ON prod.work ROLE viewer PASSWORD 'correct horse battery';
             GRANT read ON notes TO ada;"
        ))
        .unwrap();
    }

    let mut ada = Session::new(&store);
    ada.sign_in("ada", "correct horse battery").unwrap();
    ada.run(USING).unwrap();

    // Ada holds a grant, so the loop over what a statement reaches is not
    // vacuous for her — which is the only condition under which this asserts
    // anything. Without the arm in `reach.rs` the subject would name no table,
    // the loop would find nothing to check, and this would answer.
    let refused = refusal(&mut ada, "INFO FOR RECIPIENTS OF team:'github';");
    assert!(refused.contains("team"), "{refused}");

    // The control: the refusal is about the grant and not about the statement
    // being unusable by anyone but an owner.
    {
        let mut root = Session::new(&store);
        root.sign_in("root", "correct horse battery").unwrap();
        root.run(&format!("{USING} GRANT read ON team TO ada;"))
            .unwrap();
    }
    let mut ada = Session::new(&store);
    ada.sign_in("ada", "correct horse battery").unwrap();
    ada.run(USING).unwrap();
    assert_eq!(listed(&mut ada).len(), 1);
}

#[test]
fn a_table_that_is_not_a_vault_has_no_recipients() {
    let store = holding();
    let mut session = session_on(&store);
    session
        .run("DEFINE TABLE notes SCHEMALESS; CREATE notes:1 = { body: 'hello' };")
        .unwrap();

    let refused = refusal(&mut session, "INFO FOR RECIPIENTS OF notes:1;");
    assert!(refused.contains("notes"), "{refused}");

    let added = refusal(&mut session, "ADD RECIPIENT 'ops' TO notes:1 KEY 0x00;");
    assert!(added.contains("notes"), "{added}");
}

#[test]
fn the_material_may_arrive_as_a_parameter() {
    let store = holding();
    let mut session = session_on(&store);

    // The shape a client actually uses: the wrapping happened somewhere else and
    // the bytes arrive bound, not formatted into the statement as hex.
    let parameters = Parameters::from([("wrapped".to_owned(), Value::Bytes(vec![9, 8, 7]))]);
    session
        .run_with(
            "ADD RECIPIENT 'ops' TO team:'github' KEY $wrapped;",
            &parameters,
        )
        .unwrap();

    assert_eq!(
        listed(&mut session).get("ops"),
        Some(&Value::Bytes(vec![9, 8, 7])),
    );
}

/// The vault record's address, resolved from the catalog.
fn address_of(store: &Store) -> RecordAddress {
    let mut transaction = store.begin().unwrap();
    let namespace = Catalog::new(&mut transaction)
        .namespaces()
        .unwrap()
        .into_iter()
        .find(|namespace| namespace.name == "prod")
        .expect("no namespace")
        .id;
    let database = Catalog::new(&mut transaction)
        .databases_in(namespace)
        .unwrap()
        .into_iter()
        .find(|database| database.name == "work")
        .expect("no database")
        .id;
    let table = Catalog::new(&mut transaction)
        .table_id(namespace, database, "team")
        .unwrap()
        .expect("no vault");
    RecordAddress::new(
        namespace,
        database,
        table,
        RecordId::Text("github".to_owned()),
    )
}

#[test]
fn a_concurrent_add_and_remove_leave_a_consistent_set() {
    let store = holding();
    {
        let mut setup = session_on(&store);
        setup
            .run("ADD RECIPIENT 'ops' TO team:'github' KEY 0xaaaa;")
            .unwrap();
    }
    let address = address_of(&store);

    // # Why this reaches past the statements
    //
    // A script cannot hold a transaction open across two `run` calls — an
    // unclosed one is refused — so the language has no way to express two
    // overlapping edits, and the interleaving F2 asks about is a property of the
    // transaction underneath. This applies exactly the change the statements
    // apply, through the same two functions, and lets the store decide.
    let mut adding = store.begin().unwrap();
    let mut removing = store.begin().unwrap();

    for (transaction, change) in [(&mut adding, true), (&mut removing, false)] {
        let stored = transaction.get(&address).unwrap().expect("no record");
        let Value::Object(mut held) = decode_payload(&stored).unwrap() else {
            panic!("not an object");
        };
        if change {
            tessari_storage::add_recipient(&mut held, "team", "audit", Value::Bytes(vec![0xbb]))
                .unwrap();
        } else {
            tessari_storage::remove_recipient(&mut held, "team", "ops").unwrap();
        }
        transaction.put(
            address.clone(),
            tessari_encoding::encode_payload(&Value::Object(held)).into_bytes(),
        );
    }

    adding.commit().unwrap();
    let second = removing.commit();

    // First committer wins and the second is **refused** rather than merged.
    // What matters is that the loser is told: a revocation that lost a race and
    // reported success is the same failure as one that matched nothing.
    assert!(
        second.is_err(),
        "the second transaction committed over the first",
    );

    let mut reader = session_on(&store);
    assert_eq!(
        listed(&mut reader).keys().cloned().collect::<Vec<_>>(),
        vec!["audit".to_owned(), "ops".to_owned()],
        "the surviving set is not the winner's",
    );

    // And the record still opens: no interleaving touched the sealed material.
    let Value::Object(opened) = value(&mut reader, "REVEAL token FROM team:'github';") else {
        panic!("expected an object");
    };
    assert_eq!(
        opened.get("token"),
        Some(&Value::String(PLANTED.to_owned())),
    );
}
