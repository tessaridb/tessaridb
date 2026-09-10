//! A vault's refusals do not quote the value they refused.
//!
//! # Why this file exists
//!
//! Criterion W4 has been argued for since the vault was designed, and the
//! argument is a good one: the variants that render a value render it only
//! where the caller supplied it, and by then the message tells them nothing
//! they did not already have. `NotCastable` carries that argument in its own
//! doc comment — and then says, correctly, that **an argument is not a test**.
//!
//! So this is the test. A known secret is submitted through every shape that
//! refuses it, and the whole refusal is scanned for the string. Both renderings
//! are scanned, `Display` and `Debug`, because the two are different code and
//! the second is what a `tracing` field or a `.unwrap()` panic prints — a
//! message that is careful in one and careless in the other is careless where
//! it matters, since the careless one is the one that reaches the log.
//!
//! The scan is only as good as its ability to find the string, so a control
//! asserts it does. Every other test here would pass against a scanner that
//! never matches anything.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

/// The secret. Long and distinctive: a short word would appear by coincidence
/// in a message that never saw the value.
const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const PASSWORD: &str = "correct horse battery";
const PASSPHRASE: &str = "an operator passphrase";
const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// Tenancy, an unsealed store, two vaults and one record holding the secret.
///
/// The second vault declares its secret field `TYPE int`, which is the only way
/// to reach the schema refusal with a string in hand — and that refusal is the
/// one variant in the write path that renders a value for every other table in
/// the store.
///
/// No user is declared here, deliberately: declaring the store's *first* user
/// closes every anonymous session, including this one, so a fixture that ends
/// with `DEFINE USER` hands back a session whose next statement is
/// `NotSignedIn`. The one test that needs users declares them at the point it
/// needs them.
fn holding(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE work; USE DATABASE work;
             UNSEAL VAULT WITH '{PASSPHRASE}';
             DEFINE VAULT team;
             DEFINE FIELD login ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;
             DEFINE VAULT counted;
             DEFINE FIELD tally ON counted TYPE int SECRET;
             CREATE team:'github' = {{ login: 'boog', token: '{PLANTED}' }};"
        ))
        .unwrap();
    session
}

/// Everything a refusal renders: the sentence and the structure behind it.
fn refusal(session: &mut Session<'_>, statement: &str) -> String {
    let error = session
        .run(statement)
        .expect_err("the statement was accepted");
    format!("{error} :: {error:?}")
}

/// Assert one refusal is free of the secret, naming the statement when it is not.
fn withholds(said: &str, statement: &str) {
    assert!(
        !said.contains(PLANTED),
        "the refusal of `{statement}` quoted the secret: {said}"
    );
}

#[test]
fn the_scan_would_find_the_secret_if_a_refusal_carried_it() {
    // The control. Without it every assertion below passes against a scan that
    // cannot match, which is the failure mode a negative test is prone to.
    let said = format!("no such thing as `{PLANTED}` (at 1:1)");
    assert!(said.contains(PLANTED));
}

#[test]
fn no_refusal_of_a_write_quotes_the_secret_it_refused() {
    let store = store();
    let mut session = holding(&store);
    session.run("SEAL VAULT;").unwrap();

    // A write the store cannot seal, holding the value it cannot seal.
    let sealed = format!("CREATE team:'gitlab' = {{ login: 'boog', token: '{PLANTED}' }};");
    withholds(&refusal(&mut session, &sealed), &sealed);

    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();

    // The declared type is checked on the way in, where the value still is one.
    // Every other table in this store gets `found` and the value both; a vault
    // gets the type alone, and this is the assertion behind that decision.
    let mistyped = format!("CREATE counted:'a' = {{ tally: '{PLANTED}' }};");
    withholds(&refusal(&mut session, &mistyped), &mistyped);

    // The caller supplying its own key set, with the secret inside it.
    let reserved = format!("CREATE team:'own' = {{ login: 'boog', \"#keys\": '{PLANTED}' }};");
    withholds(&refusal(&mut session, &reserved), &reserved);
}

#[test]
fn no_refusal_of_a_read_quotes_the_secret_it_withheld() {
    let store = store();
    let mut session = holding(&store);

    // `SELECT` over a vault is refused and names `REVEAL`. The record is there
    // and holds the secret, so a refusal built by rendering the row would carry
    // it.
    let selected = "SELECT * FROM team;";
    withholds(&refusal(&mut session, selected), selected);
    let projected = "SELECT token FROM team:'github';";
    withholds(&refusal(&mut session, projected), projected);

    // Sealed: the ciphertext is in hand, the key is not.
    session.run("SEAL VAULT;").unwrap();
    let revealed = "REVEAL token FROM team:'github';";
    withholds(&refusal(&mut session, revealed), revealed);

    // Unreachable: the key is in hand, the grant is not. `ada` is granted on
    // `notes` deliberately — a user with no grants at all passes the reach
    // check vacuously, so a refusal collected from one would prove nothing.
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();

    // Declaring the first user closes this anonymous session, so it is the last
    // thing this one does.
    session
        .run(&format!(
            "DEFINE TABLE notes SCHEMALESS;
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(&format!(
        "{USING}
         DEFINE USER ada ON prod.work ROLE editor PASSWORD '{PASSWORD}';
         GRANT read, write ON notes TO ada;"
    ))
    .unwrap();
    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).unwrap();
    ada.run(USING).unwrap();
    withholds(&refusal(&mut ada, revealed), revealed);
}

#[test]
fn no_refusal_of_a_recipient_change_quotes_what_the_record_holds() {
    let store = store();
    let mut session = holding(&store);

    // A recipient is named by text, and the name is an expression — so a caller
    // can write something there that carries a value. This one carries the
    // secret, and the refusal answers with the word `object`. Reporting the
    // **type** is the whole of the decision behind that variant, and this is the
    // shape it was made for.
    let objected = format!("ADD RECIPIENT {{ token: '{PLANTED}' }} TO team:'github' KEY 0x00;");
    let said = refusal(&mut session, &objected);
    withholds(&said, &objected);
    assert!(
        said.contains("object"),
        "the refusal should name the type it was given: {said}"
    );

    // A literal, chosen long enough that it cannot collide with a line, column
    // or byte offset the span renders.
    let numbered = "ADD RECIPIENT 987654321 TO team:'github' KEY 0x00;";
    let said = refusal(&mut session, numbered);
    withholds(&said, numbered);
    assert!(
        !said.contains("987654321"),
        "the refusal named the value it was given: {said}"
    );

    // Worth recording where this does NOT reach: a bare identifier here is a
    // *table* reference, so `ADD RECIPIENT token …` is refused earlier with
    // `no table named "token"` and never reaches the recipient check at all.
    // The variant's doc comment imagines a field reference quoting whatever the
    // field holds; that form is not expressible in this grammar today, and the
    // object above is the reachable version of the same risk.
}

#[test]
fn a_wrong_passphrase_is_not_echoed_by_the_refusal() {
    let store = store();
    let mut session = holding(&store);
    session.run("SEAL VAULT;").unwrap();

    // A passphrase is a secret submitted by a caller who already knows it, so
    // by the usual argument echoing it costs nothing. It is echoed into a log
    // the caller does not own, which is where the argument stops holding.
    let wrong = format!("UNSEAL VAULT WITH '{PLANTED}';");
    withholds(&refusal(&mut session, &wrong), &wrong);
}
