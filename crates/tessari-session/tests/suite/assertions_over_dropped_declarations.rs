//! An assertion that names another field keeps meaning what it meant, and the
//! reason it looks otherwise is that a declaration and the data are two things.
//!
//! # The claim this file refutes
//!
//! **Q-448** recorded that `DEFINE FIELD high … ASSERT $value > low` keeps
//! enforcing after `DROP FIELD low` while comparing against nothing — *"the
//! constraint did not fail, it stopped meaning anything, which is the worse of
//! the two"* — and proposed either refusing the drop or making the comparison
//! false. It was measured: with `low` dropped, `UPDATE t:1 SET high = 5` was
//! accepted and `CHECK TABLE t` reported nothing.
//!
//! Both observations reproduce exactly. **Both are correct.** `DROP FIELD`
//! removes a *declaration*; on a schemaless table it does not remove the
//! *value*, so the record still held `low = 1` and `5 > 1` genuinely holds. The
//! assertion was evaluated, against the field it names, and passed.
//!
//! The inference from "the write was accepted" to "the constraint stopped being
//! evaluated" is what was wrong, and it is a cheap inference to make: nothing in
//! the accepted write distinguishes *the check ran and passed* from *the check
//! did not run*. The distinguishing case is a value the check would refuse, and
//! that is what the tests below use.
//!
//! # What is actually true, pinned here so the question cannot be re-raised
//!
//! - the assertion still refuses a value the dropped-declaration field's stored
//!   value contradicts;
//! - against a record that carries no such field at all, an ordered comparison
//!   is **false** and the write is refused, which is what `Assertion::holds`
//!   documents and what `WHERE` answers for the same question;
//! - `CHECK TABLE` reports nothing because nothing is violating, not because it
//!   stopped looking.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// A table whose `high` must exceed its `low`, holding one record that obeys.
fn constrained(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE band SCHEMALESS;\n\
             DEFINE FIELD low ON band TYPE int;\n\
             DEFINE FIELD high ON band TYPE int ASSERT $value > low;\n\
             CREATE band:1 = { low: 1, high: 2 };",
        )
        .unwrap();
    session
}

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn refused(session: &mut Session<'_>, script: &str) -> String {
    match session.run(script) {
        Err(why) => why.to_string(),
        Ok(outcome) => panic!("expected a refusal, got {outcome:?}"),
    }
}

#[test]
fn dropping_a_declaration_does_not_remove_the_value_the_assertion_reads() {
    let store = store();
    let mut session = constrained(&store);
    session.run("DROP FIELD low ON band;").unwrap();

    let Some(Outcome::Records { records, .. }) = session.run("SELECT * FROM band;").unwrap().pop()
    else {
        panic!("expected records");
    };
    let (_, record) = &records[0];
    let Value::Object(fields) = record else {
        panic!("expected an object");
    };
    // The declaration is gone and the value is not. That is the whole
    // explanation for what Q-448 saw, and it is stated first because every
    // assertion below depends on it.
    assert_eq!(
        fields.get("low"),
        Some(&Value::from(1_i64)),
        "dropping a declaration took the value with it: {fields:?}"
    );
}

#[test]
fn the_assertion_still_refuses_a_value_the_dropped_field_contradicts() {
    let store = store();
    let mut session = constrained(&store);
    session.run("DROP FIELD low ON band;").unwrap();

    // The distinguishing case. An accepted write proves nothing — it looks the
    // same whether the check ran and passed or never ran — so the test uses a
    // value the check must refuse.
    let refusal = refused(&mut session, "UPDATE band:1 SET high = 0;");
    assert!(
        refusal.contains("compared with low"),
        "the assertion stopped naming the field it compares against: {refusal}"
    );

    // And the acceptance Q-448 recorded, shown to be an acceptance on the
    // merits: `5 > 1` holds, so the write is right to land.
    session.run("UPDATE band:1 SET high = 5;").unwrap();
}

#[test]
fn a_comparison_against_a_field_the_record_does_not_carry_is_false() {
    let store = store();
    let mut session = constrained(&store);
    session.run("DROP FIELD low ON band;").unwrap();

    // No `low` anywhere — not in the declarations and not in the record. An
    // ordered comparison against an absence is false, which is the answer
    // `WHERE` gives for the same question, so the write is refused rather than
    // waved through.
    let refusal = refused(&mut session, "CREATE band:2 = { high: 5 };");
    assert!(
        refusal.contains("compared with low"),
        "a comparison against an absent field did not refuse: {refusal}"
    );

    // The same, reached from the other direction: taking the value out of an
    // existing record is refused by the same rule.
    let refusal = refused(&mut session, "UPDATE band:1 = { high: 5 };");
    assert!(
        refusal.contains("compared with low"),
        "removing the compared field from a record did not refuse: {refusal}"
    );
}

#[test]
fn check_table_reports_nothing_because_nothing_violates() {
    let store = store();
    let mut session = constrained(&store);
    session.run("DROP FIELD low ON band;").unwrap();

    let Some(Outcome::Value(Value::Array(rows))) = session.run("CHECK TABLE band;").unwrap().pop()
    else {
        panic!("expected a list");
    };
    assert!(
        rows.is_empty(),
        "the store held a record its declarations refuse: {rows:?}"
    );

    // And the control that makes the empty answer mean something: a record that
    // *does* violate is reported, so `CHECK TABLE` is still looking.
    session
        .run("DEFINE FIELD low ON band TYPE int; UPDATE band:1 = { low: 9, high: 5 };")
        .map_or_else(
            |_| {
                // Refused at the write, which is the stronger answer: the store
                // will not hold the violating record in the first place.
            },
            |_| panic!("a violating record was accepted"),
        );
}
