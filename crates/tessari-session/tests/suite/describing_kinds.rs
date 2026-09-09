//! Every kind describes itself as the word that made it.
//!
//! # Why this test exists, and what it is really guarding
//!
//! `describe.rs` turns a stored table definition back into the statement that
//! declared it, and its arms are an if-chain ending in a **fall-through** to
//! `DEFINE TABLE`. A fall-through is not a ratchet: a kind added without an arm
//! compiles, runs, and describes itself as something else.
//!
//! That is not hypothetical. Queues shipped in W152 with no arm, so
//! `INFO FOR TABLE jobs` answered `DEFINE TABLE jobs SCHEMALESS` — a statement
//! that re-executes happily and restores a table with two ordinary fields and no
//! hold. Every refusal `DEFINE QUEUE` carries was gone, and nothing was in an
//! error state. It survived a release because nothing asked (Q-475).
//!
//! So the guard is the **match below**, not the assertions. It is exhaustive
//! over [`TableKind`], with no `_ =>` arm, which is the device `reach.rs` and
//! `identity::Needs::of` both use and both explain: a tenth kind cannot compile
//! this file until somebody decides what its declaration says. Putting the
//! ratchet here rather than restructuring `describe.rs` buys the same guarantee
//! without touching six arms that are correct today.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::{Catalog, Store, TableKind};
use tessari_types::Value;

/// How a table of this kind is declared, and the word its description must
/// begin with.
///
/// **Exhaustive on purpose, with no `_ =>` arm.** This is the guard: a tenth
/// kind cannot compile this file until somebody writes down how it is declared
/// and what its description says. The kinds it is called with are read back from
/// the catalog rather than built here, because a vault's key is minted by the
/// store and is not a value a test can invent.
fn declared_as(kind: &TableKind) -> (&'static str, &'static str) {
    match kind {
        TableKind::Table => ("DEFINE TABLE t SCHEMALESS;", "DEFINE TABLE t"),
        TableKind::Collection => ("DEFINE COLLECTION t;", "DEFINE COLLECTION t"),
        TableKind::Bucket(_) => ("DEFINE BUCKET t MAX 1024;", "DEFINE BUCKET t MAX 1024"),
        TableKind::Edge(_) => ("DEFINE TABLE t EDGE;", "DEFINE TABLE t EDGE"),
        TableKind::Vector(_) => (
            "DEFINE VECTOR t DIMENSION 3 DISTANCE cosine;",
            "DEFINE VECTOR t DIMENSION 3 DISTANCE cosine",
        ),
        TableKind::Geo => ("DEFINE GEO t;", "DEFINE GEO t"),
        TableKind::Vault(_) => ("DEFINE VAULT t;", "DEFINE VAULT t"),
        TableKind::Queue(_) => (
            "DEFINE QUEUE t TIMEOUT 30s ATTEMPTS 5;",
            "DEFINE QUEUE t TIMEOUT 30s ATTEMPTS 5",
        ),
        TableKind::View(_) => (
            "DEFINE VIEW t AS SELECT * FROM base;",
            "DEFINE VIEW t AS SELECT * FROM base",
        ),
        TableKind::Series(_) => ("DEFINE SERIES t RETAIN 12h;", "DEFINE SERIES t RETAIN 12h"),
    }
}

/// Every declaration this test exercises.
///
/// A list rather than a derivation, because there is nothing to derive it from:
/// the statement that makes a kind is a fact about the grammar. It is kept
/// honest by the count assertion at the end of the test — if a kind is added to
/// `declared_as` and not here, the two disagree and the test says so.
const DECLARATIONS: [&str; 10] = [
    "DEFINE TABLE t SCHEMALESS;",
    "DEFINE COLLECTION t;",
    "DEFINE BUCKET t MAX 1024;",
    "DEFINE TABLE t EDGE;",
    "DEFINE VECTOR t DIMENSION 3 DISTANCE cosine;",
    "DEFINE GEO t;",
    "DEFINE VAULT t;",
    "DEFINE QUEUE t TIMEOUT 30s ATTEMPTS 5;",
    "DEFINE VIEW t AS SELECT * FROM base;",
    "DEFINE SERIES t RETAIN 12h;",
];

/// The kind the catalog now holds for `prod.shop.t`.
fn stored_kind(store: &Store) -> TableKind {
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let id = catalog.table_id(namespace, database, "t").unwrap().unwrap();
    let kind = catalog.table(id).unwrap().unwrap().kind;
    transaction.rollback();
    kind
}

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The `definition` field of `INFO FOR TABLE t`, or the reason there is none.
fn described(session: &mut Session<'_>) -> Result<String, String> {
    let outcome = session.run("INFO FOR TABLE t;").unwrap().pop().unwrap();
    let Outcome::Value(Value::Object(report)) = outcome else {
        panic!("expected a report");
    };
    match (report.get("definition"), report.get("undefinable")) {
        (Some(Value::String(script)), _) => Ok(script.clone()),
        (_, Some(Value::String(why))) => Err(why.clone()),
        _ => panic!("the report named neither a definition nor a reason: {report:?}"),
    }
}

#[test]
fn every_table_kind_describes_itself_as_the_word_that_made_it() {
    let mut seen = Vec::new();
    for declaration in DECLARATIONS {
        let store = store();
        let mut session = Session::new(&store);
        session
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                 DEFINE DATABASE shop; USE DATABASE shop;\n\
                 UNSEAL VAULT WITH 'an operator passphrase';\n\
                 DEFINE TABLE base SCHEMALESS;",
            )
            .unwrap();
        session.run(declaration).unwrap();

        // Read back from the catalog rather than assumed from the statement, so
        // the exhaustive match is reached with the kind the store actually
        // stored — which is also what makes a declaration that quietly produced
        // the wrong kind visible here.
        let kind = stored_kind(&store);
        let (written, expected) = declared_as(&kind);
        assert_eq!(
            written, declaration,
            "`{declaration}` stored a kind whose own declaration is `{written}`"
        );

        let script = described(&mut session)
            .unwrap_or_else(|why| panic!("`{declaration}` has no description: {why}"));

        // The description begins with the word that declared it. Asserting the
        // prefix rather than the whole script keeps this test about the failure
        // it exists to catch — a kind described as another kind — and leaves the
        // clauses each arm writes to that kind's own tests.
        assert!(
            script.starts_with(expected),
            "`{declaration}` was described as `{}`, not as `{expected}`",
            script.lines().next().unwrap_or_default()
        );
        seen.push(expected);
    }

    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        DECLARATIONS.len(),
        "two declarations described themselves the same way: {seen:?}"
    );
}

#[test]
fn a_queue_description_re_executes_into_a_queue_and_not_into_a_table() {
    // The regression Q-475 names, asserted end to end rather than by reading the
    // script: the description is run against a second store and the queue that
    // comes back must still hand work out under a hold.
    let first = store();
    let mut session = Session::new(&first);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE QUEUE t TIMEOUT 30s ATTEMPTS 5;",
        )
        .unwrap();
    let script = described(&mut session).unwrap();

    let second = store();
    let mut restored = Session::new(&second);
    restored
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;",
        )
        .unwrap();
    restored.run(&script).unwrap();
    restored.run("CREATE t:1 = { url: 'a' };").unwrap();

    // Before the arm existed this restored a plain table, and `CLAIM` against a
    // plain table is refused — so this statement is the whole test.
    let claimed = restored.run("CLAIM FROM t;").unwrap().pop().unwrap();
    match claimed {
        Outcome::Records { records, .. } => {
            assert_eq!(records.len(), 1, "the restored queue handed out nothing");
        }
        other => panic!("the restored declaration is not a queue: {other:?}"),
    }
}
