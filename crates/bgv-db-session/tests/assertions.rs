//! `ASSERT` — what a field demands of its value beyond its type.
//!
//! The wave's whole argument is about **where** the check runs. It runs on the
//! store's apply path, beside the type check, so that a replica reaches the same
//! verdict from the record alone and a refusal fails the whole commit. The store
//! sits below the language and cannot parse bgvQL, so what it holds is the
//! **lowered** constraint rather than the sentence that described it — and the
//! comparison it makes is the one a `WHERE` makes, because it is the same
//! function.
//!
//! That last point is the one this file can actually prove from out here, and it
//! is what `assert_and_where_cannot_disagree` does.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::Session;
use bgv_db_storage::Store;
use bgv_db_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE bank; USE DATABASE bank;\n\
             DEFINE TABLE accounts;\n\
             DEFINE FIELD balance ON accounts TYPE int ASSERT $value >= 0;",
        )
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

#[test]
fn a_value_the_declaration_refuses_does_not_land() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE accounts:1 = { balance: 10 };").unwrap();
    let refused = session.run("CREATE accounts:2 = { balance: -1 };");
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        ids(&mut session, "SELECT * FROM accounts;"),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn a_refusal_fails_the_whole_commit() {
    // The same rule every other schema violation follows: a transaction never
    // lands half constrained, so the conforming write beside the refused one is
    // not there either.
    let store = store();
    let mut session = ready(&store);
    let refused = session.run(
        "BEGIN;\n\
         CREATE accounts:1 = { balance: 10 };\n\
         CREATE accounts:2 = { balance: -5 };\n\
         COMMIT;",
    );
    assert!(refused.is_err(), "{refused:?}");
    assert!(ids(&mut session, "SELECT * FROM accounts;").is_empty());
}

#[test]
fn an_absent_field_and_a_null_one_pass() {
    // An assertion constrains a present, non-null value, exactly as a type does.
    // `REQUIRED` is the one constraint about absence, and an assertion that also
    // implied presence would make `REQUIRED` mean two things depending on what
    // stood beside it.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE accounts:1 = { holder: 'ada' };\n\
             CREATE accounts:2 = { balance: NULL };",
        )
        .unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM accounts;"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn declaring_one_over_rows_that_violate_it_is_refused_and_writes_nothing() {
    // The second pass `schema.rs` already runs for a type. Without it a
    // constraint could be declared over data that breaks it, and every reader
    // afterwards would believe it held.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE bank; USE DATABASE bank;\n\
             DEFINE TABLE accounts;\n\
             CREATE accounts:1 = { balance: 10 };\n\
             CREATE accounts:2 = { balance: -5 };",
        )
        .unwrap();

    let refused = session.run("DEFINE FIELD balance ON accounts TYPE int ASSERT $value >= 0;");
    assert!(refused.is_err(), "{refused:?}");
    // …and the declaration is not there either, so a later write is unconstrained
    // rather than constrained by something that never applied.
    session.run("CREATE accounts:3 = { balance: -7 };").unwrap();
}

#[test]
fn a_range_is_two_comparisons_and_a_set_is_one_membership() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;\n\
             DEFINE FIELD age ON orders TYPE int ASSERT $value > 0 AND $value < 150;\n\
             DEFINE FIELD status ON orders TYPE string ASSERT $value IN ['new', 'paid'];",
        )
        .unwrap();
    session
        .run("CREATE orders:1 = { age: 30, status: 'paid' };")
        .unwrap();
    for refused in [
        "CREATE orders:2 = { age: 0 };",
        "CREATE orders:3 = { age: 200 };",
        "CREATE orders:4 = { status: 'cancelled' };",
    ] {
        assert!(session.run(refused).is_err(), "{refused} was accepted");
    }
    assert_eq!(
        ids(&mut session, "SELECT * FROM orders;"),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn assert_and_where_cannot_disagree() {
    // The reason the comparison module moved below both crates. If the store
    // re-implemented `>` the two would eventually disagree, and the disagreement
    // would be a write one node refuses and another accepts.
    //
    // Asserted rather than argued: every value that a `WHERE` says satisfies the
    // comparison is exactly a value the assertion admits, over a deliberately
    // awkward set — a string against a number (the declared order crosses
    // types), a null, an absent field.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE t; USE DATABASE t;\n\
             DEFINE TABLE loose;\n\
             CREATE loose:1 = { v: 5 };\n\
             CREATE loose:2 = { v: -5 };\n\
             CREATE loose:3 = { v: 0 };\n\
             CREATE loose:4 = { v: 'nineteen' };\n\
             CREATE loose:5 = { v: NULL };\n\
             CREATE loose:6 = { holder: 'no v at all' };",
        )
        .unwrap();
    let filtered = ids(&mut session, "SELECT * FROM loose WHERE v > 0;");

    // The same six records, one at a time, against a table whose declaration is
    // the same comparison. What lands is what the filter found — except that the
    // filter cannot see a record whose value is absent or null, while the
    // assertion admits both by the presence rule, so those two are held out and
    // asserted separately below.
    session
        .run("DEFINE TABLE strict;\nDEFINE FIELD v ON strict TYPE any ASSERT $value > 0;")
        .unwrap();
    let mut admitted = Vec::new();
    for (id, written) in [
        (1, "5"),
        (2, "-5"),
        (3, "0"),
        (4, "'nineteen'"),
        (5, "NULL"),
        (6, "none-at-all"),
    ] {
        let script = if written == "none-at-all" {
            format!("CREATE strict:{id} = {{ holder: 'x' }};")
        } else {
            format!("CREATE strict:{id} = {{ v: {written} }};")
        };
        if session.run(&script).is_ok() {
            admitted.push(RecordId::Int(id));
        }
    }

    // Absent and null are admitted by the presence rule and are invisible to an
    // ordered filter, so they are the two the two paths are *allowed* to differ
    // on — and they are named here rather than quietly subtracted.
    let comparable: Vec<RecordId> = admitted
        .iter()
        .filter(|id| **id != RecordId::Int(5) && **id != RecordId::Int(6))
        .cloned()
        .collect();
    assert_eq!(comparable, filtered, "admitted {admitted:?}");
    assert!(admitted.contains(&RecordId::Int(5)), "{admitted:?}");
    assert!(admitted.contains(&RecordId::Int(6)), "{admitted:?}");
    // And the awkward one is in both: a string ranks above a number in the
    // declared order, so `'nineteen' > 0` holds in a filter and in an assertion.
    assert!(filtered.contains(&RecordId::Int(4)), "{filtered:?}");
}

#[test]
fn an_assertion_outside_the_vocabulary_is_refused_where_it_is_written() {
    // Refusing here rather than at evaluation is the whole design: the store is
    // then incapable of meeting an assertion it cannot check, so there is no
    // runtime branch for one and no way for two replicas to differ over what a
    // constraint meant.
    let store = store();
    let mut session = ready(&store);
    for script in [
        // A call — `time::now()` is not a function of the record, so two
        // replicas would reach different verdicts.
        "DEFINE FIELD opened ON accounts TYPE datetime ASSERT $value < time::now();",
        // A computed right-hand side.
        "DEFINE FIELD n ON accounts TYPE int ASSERT $value > 1 + 1;",
        // A field of the record — an assertion is over one value, and a
        // cross-field one is its own question.
        "DEFINE FIELD low ON accounts TYPE int ASSERT $value < high;",
        // Any parameter but `$value`: nothing can bind it, which is the rule
        // `DEFAULT` already follows.
        "DEFINE FIELD m ON accounts TYPE int ASSERT $limit > 3;",
        // The mirror spelling, refused rather than flipped — flipping is only
        // correct for the ordered operators.
        "DEFINE FIELD k ON accounts TYPE int ASSERT 0 < $value;",
        // A bare literal is not a constraint on anything.
        "DEFINE FIELD j ON accounts TYPE int ASSERT true;",
    ] {
        let refused = session.run(script);
        assert!(refused.is_err(), "{script} was accepted: {refused:?}");
    }
}

#[test]
fn an_assertion_survives_a_reopen_because_it_is_catalog_and_not_memory() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    {
        let store = Store::open(Arc::clone(&backend)).unwrap();
        ready(&store);
    }
    let store = Store::open(backend).unwrap();
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE bank;")
        .unwrap();
    assert!(
        session.run("CREATE accounts:9 = { balance: -1 };").is_err(),
        "the assertion did not come back with the catalog"
    );
    session.run("CREATE accounts:9 = { balance: 1 };").unwrap();
}
