//! What the wire protocol carries back about a vault.
//!
//! Survey row 20 — the wire encoder — was dispositioned `tested` in a wave that
//! never ran (Q-424). This is that test.
//!
//! # What is scanned, and why it is the decoded answers
//!
//! The row is about the **encoder**, so what matters is everything it put on the
//! wire. The answers a client decodes are exactly that, round-tripped: a field
//! the encoder wrote is a field the client holds, and a field it did not write
//! is one the client cannot invent. Scanning the whole decoded set with its
//! `Debug` rendering therefore covers every field of every answer — including
//! the ones no assertion names, which is the point, because a leak arrives in
//! the field nobody thought to check.
//!
//! `Debug` rather than `Display` for the same reason it is used in the session's
//! refusal tests: `Debug` is what a tracing field and a panic print, so a value
//! that is hidden from one rendering and present in the other is still a value
//! that reaches a log.

#![allow(clippy::panic, clippy::unwrap_used)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::sync::Arc;

use tessari_wire::{Client, Node};
use tessaridb::Db;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";
const PASSPHRASE: &str = "an operator passphrase 4b71";
const CONTROL: &str = "ada-lovelace-marker-71c4";

fn serving(db: Db) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(Arc::new(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || serve_until_the_test_ends(&held)));
    (node, address)
}

const TENANCY: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// Everything a probe needs, and the control beside it.
fn ready(client: &mut Client) {
    client
        .run(
            &format!(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;
                 DEFINE DATABASE work; USE DATABASE work;
                 UNSEAL VAULT WITH '{PASSPHRASE}';
                 DEFINE VAULT team;
                 DEFINE FIELD login ON team TYPE string;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};
                 DEFINE TABLE notes SCHEMALESS;
                 CREATE notes:1 = {{ text: '{CONTROL}' }};"
            ),
            None,
        )
        .unwrap();
}

/// What this client says it received, whole — every field of every answer.
fn everything(client: &mut Client, script: &str) -> String {
    match client.run(script, None) {
        Ok(answers) => format!("{answers:?}"),
        // A refusal is an outcome the encoder produced too, and it is the one
        // most likely to carry a value it was given.
        Err(refusal) => format!("{refusal} :: {refusal:?}"),
    }
}

#[test]
fn only_a_reveal_puts_a_secret_on_the_wire() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    ready(&mut client);

    // The control: this encoder does carry a value across when a statement
    // answers with one.
    let said = everything(&mut client, &format!("{TENANCY} SELECT * FROM notes;"));
    assert!(
        said.contains(CONTROL),
        "the encoder carried no value: {said}"
    );

    // And the one statement that may answer with a plaintext does, so the
    // absences below are about the encoder rather than about an empty store.
    let said = everything(
        &mut client,
        &format!("{TENANCY} REVEAL token FROM team:'github';"),
    );
    assert!(said.contains(PLANTED), "`REVEAL` carried no secret: {said}");

    for probe in [
        "SELECT * FROM team;",
        "SELECT * FROM team:'github';",
        "SELECT * FROM team WHERE token = 'guess';",
        "REVEAL login FROM team:'github';",
        "INFO FOR VAULT team;",
        "INFO FOR TABLE team;",
        "INFO FOR RECIPIENTS OF team:'github';",
        "DEFINE INDEX by_token ON team FIELDS token;",
        "ALTER TABLE team SET SCHEMALESS;",
        "CREATE team:'gitlab' = { login: 'boog', recovery: 'anything' };",
    ] {
        let said = everything(&mut client, &format!("{TENANCY} {probe}"));
        assert!(
            !said.contains(PLANTED),
            "`{probe}` put a secret on the wire: {said}"
        );
        assert!(
            !said.contains(PASSPHRASE),
            "`{probe}` echoed the unseal passphrase: {said}"
        );
    }

    // The passphrase carried *in* rather than out: a wrong one must not come
    // back inside the refusal that rejected it.
    let said = everything(
        &mut client,
        &format!("{TENANCY} UNSEAL VAULT WITH 'not-{PASSPHRASE}';"),
    );
    assert!(
        !said.contains(PASSPHRASE),
        "the refusal carried the passphrase it was given: {said}"
    );
}

/// Serve `node` on a runtime of this test's own: the node creates none.
fn serve_until_the_test_ends(node: &Node) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
}

// The vault frame (ADR-0092 D2): unseal, seal and ask with the passphrase as a
// field of its own, never script text, and never in what comes back.

fn state(answer: &tessari_wire::Answer) -> String {
    let rendered = format!("{answer:?}");
    for state in ["uninitialised", "unsealed", "sealed"] {
        if rendered.contains(&format!("\"{state}\"")) {
            return state.to_owned();
        }
    }
    panic!("no state in the answer: {rendered}");
}

#[test]
fn the_vault_frame_unseals_seals_and_reports() {
    use tessari_wire::VaultCall;
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    assert_eq!(
        state(&client.vault(&VaultCall::Status, None, None).unwrap()),
        "uninitialised"
    );

    let unsealed = client
        .vault(&VaultCall::Unseal(PASSPHRASE.to_owned()), None, None)
        .unwrap();
    assert_eq!(state(&unsealed), "unsealed");
    assert!(format!("{unsealed:?}").contains("initialised"));
    assert!(
        !format!("{unsealed:?}").contains(PASSPHRASE),
        "the passphrase came back"
    );

    assert_eq!(
        state(&client.vault(&VaultCall::Seal, None, None).unwrap()),
        "sealed"
    );

    let refused = client
        .vault(
            &VaultCall::Unseal("not the passphrase".to_owned()),
            None,
            None,
        )
        .expect_err("a wrong passphrase unsealed");
    assert!(
        !format!("{refused} {refused:?}").contains("not the passphrase"),
        "the guess came back: {refused:?}"
    );

    // The connection is still a conversation afterwards: a refused vault act
    // does not end it, exactly as a refused statement does not.
    assert_eq!(
        state(&client.vault(&VaultCall::Status, None, None).unwrap()),
        "sealed"
    );
    client.run("INFO FOR SEAL;", None).unwrap();
}

#[test]
fn a_vault_call_never_prints_its_passphrase() {
    let call = tessari_wire::VaultCall::Unseal(PASSPHRASE.to_owned());
    assert!(!format!("{call:?}").contains(PASSPHRASE), "{call:?}");
}

#[test]
fn the_vault_frame_changes_the_passphrase() {
    use tessari_wire::VaultCall;
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .vault(&VaultCall::Unseal(PASSPHRASE.to_owned()), None, None)
        .unwrap();
    let changed = client
        .vault(
            &VaultCall::Change {
                current: PASSPHRASE.to_owned(),
                new: "the next one 9c02".to_owned(),
            },
            None,
            None,
        )
        .unwrap();
    assert!(
        !format!("{changed:?}").contains("the next one"),
        "{changed:?}"
    );
    client.vault(&VaultCall::Seal, None, None).unwrap();
    assert!(
        client
            .vault(&VaultCall::Unseal(PASSPHRASE.to_owned()), None, None)
            .is_err()
    );
    assert_eq!(
        state(
            &client
                .vault(
                    &VaultCall::Unseal("the next one 9c02".to_owned()),
                    None,
                    None
                )
                .unwrap()
        ),
        "unsealed"
    );
}

// One vault carrying its own passphrase, reached by the same frame with a
// target (ADR-0093 D6). The target never becomes the connection's `USE`.

#[test]
fn the_vault_frame_reaches_one_vault_and_leaves_the_connection_where_it_was() {
    use tessari_wire::{VaultCall, VaultPlace};
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut setup = Client::connect(&address).unwrap();
    setup
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work; USE DATABASE work;
             DEFINE VAULT team PASSPHRASE 'the team passphrase 5d13';",
            None,
        )
        .unwrap();
    let team = VaultPlace {
        namespace: "prod".to_owned(),
        database: "work".to_owned(),
        vault: "team".to_owned(),
    };

    let mut client = Client::connect(&address).unwrap();
    let status = client.vault(&VaultCall::Status, Some(&team), None).unwrap();
    assert_eq!(state(&status), "unsealed");
    assert!(format!("{status:?}").contains("\"own\""), "{status:?}");
    assert_eq!(
        state(&client.vault(&VaultCall::Seal, Some(&team), None).unwrap()),
        "sealed"
    );
    let refused = client
        .vault(
            &VaultCall::Unseal("a wrong one 3e11".to_owned()),
            Some(&team),
            None,
        )
        .expect_err("a wrong vault passphrase unsealed");
    assert!(
        !format!("{refused} {refused:?}").contains("a wrong one"),
        "{refused:?}"
    );
    assert_eq!(
        state(
            &client
                .vault(
                    &VaultCall::Unseal("the team passphrase 5d13".to_owned()),
                    Some(&team),
                    None
                )
                .unwrap()
        ),
        "unsealed"
    );
    // The store itself was never initialised by any of that.
    assert_eq!(
        state(&client.vault(&VaultCall::Status, None, None).unwrap()),
        "uninitialised"
    );
    // And this connection selected no tenancy along the way.
    assert!(
        client.run("INFO FOR SEAL OF team;", None).is_err(),
        "the frame's target became the connection's USE"
    );
}

#[test]
fn a_closed_store_refuses_every_vault_act_to_nobody_and_every_change_to_a_viewer() {
    use tessari_wire::VaultCall;
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .run("DEFINE USER root ROLE owner PASSWORD 'root secret';", None)
        .unwrap();
    client
        .run(
            "DEFINE USER grace ROLE viewer PASSWORD 'watch only';",
            Some(("root", "root secret")),
        )
        .unwrap();

    let acts = [
        VaultCall::Status,
        VaultCall::Unseal(PASSPHRASE.to_owned()),
        VaultCall::Seal,
        VaultCall::Change {
            current: PASSPHRASE.to_owned(),
            new: "another".to_owned(),
        },
    ];
    // A connection is a session, and the one above is now root's: each identity
    // asks on a connection of its own.
    drop(client);
    for act in &acts {
        let mut nobody = Client::connect(&address).unwrap();
        let refused = nobody
            .vault(act, None, None)
            .expect_err("nobody was answered by a closed store");
        assert!(refused.to_string().contains("sign"), "{act:?}: {refused}");
    }
    // A viewer may ask whether secrets can be opened (ADR-0092 D1) and may do
    // nothing to the key.
    let mut grace = Client::connect(&address).unwrap();
    grace
        .vault(&VaultCall::Status, None, Some(("grace", "watch only")))
        .expect("a signed-in caller may ask the state");
    for act in acts.iter().skip(1) {
        let refused = grace
            .vault(act, None, Some(("grace", "watch only")))
            .expect_err("a viewer acted on the store's key");
        assert!(
            refused.to_string().contains("operate"),
            "{act:?}: {refused}"
        );
    }
}
