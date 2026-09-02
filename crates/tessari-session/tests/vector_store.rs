//! `DEFINE VECTOR` — a store of vectors, worked with the way a store is.
//!
//! # What the word adds over the three statements it stands for
//!
//! ```text
//! DEFINE COLLECTION embeddings;
//! DEFINE FIELD vector ON embeddings TYPE vector<3> REQUIRED;
//! DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine;
//! ```
//!
//! Three statements that only work when all three are right. A width with no
//! index declares something nothing searches; an index with no width admits a
//! row of the wrong shape and reports it as infinitely far from everything;
//! neither without `REQUIRED` admits a record with no vector at all — legal in a
//! table, and not a record of a vector store. The word makes the three
//! inseparable, and `INFO` answers with the word rather than with the three.
//!
//! # The property this file exists to hold
//!
//! **One code path.** The store desugars into those same three statements
//! through the same three functions the long spellings use, so a width declared
//! by the word is refused by the same check that refuses one declared by the
//! field. The test that matters is not that each refuses — it is that the two
//! refuse the *same write* with the *same words*, character for character. A
//! second implementation would agree on the happy path and part company on the
//! day the data does not fit, which is the day nobody is watching.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE library; USE DATABASE library;
";

/// A session holding a three-wide cosine store called `embeddings`.
fn declared(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE VECTOR embeddings DIMENSION 3 DISTANCE cosine;"
        ))
        .unwrap();
    session
}

/// What a statement said when it was refused, or `None` when it was accepted.
fn refusal(session: &mut Session<'_>, statement: &str) -> Option<String> {
    session.run(statement).err().map(|error| error.to_string())
}

/// The object a statement answered with.
fn reported(session: &mut Session<'_>, statement: &str) -> Value {
    match session.run(statement).unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

#[test]
fn a_vector_store_takes_records_and_gives_them_back() {
    // The whole of what the owner asked for, in four statements: declare it,
    // write into it, read out of it.
    let held = store();
    let mut session = declared(&held);

    session
        .run("CREATE embeddings:'intro' = { vector: [1.0, 0.0, 0.0], label: 'intro' };")
        .unwrap();
    session
        .run("CREATE embeddings:'body' = { vector: [0.0, 1.0, 0.0], label: 'body' };")
        .unwrap();

    let Outcome::Records { records, .. } = session
        .run("SELECT * FROM embeddings;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a select answered with something other than records");
    };
    assert_eq!(records.len(), 2, "{records:?}");
}

#[test]
fn the_store_and_the_field_refuse_the_same_write_with_the_same_words() {
    // The criterion, and the only test here that could not be written any other
    // way. Same record, same value, same width — so the two messages are
    // comparable character for character and any difference at all is the two
    // doorways having become two implementations.
    // The id is quoted because a bare word is not a record id: with `one` both
    // sides died in the parser with the same message, and the assertion below
    // compared that message with itself.
    let bad = "CREATE embeddings:'one' = { vector: [1.0, 2.0] };";

    let one = store();
    let from_the_store = refusal(&mut declared(&one), bad).expect("the store accepted it");

    let two = store();
    let mut session = Session::new(&two);
    session
        .run(&format!(
            "{PLACE}DEFINE COLLECTION embeddings;\n\
             DEFINE FIELD vector ON embeddings TYPE vector<3>;"
        ))
        .unwrap();
    let from_the_field = refusal(&mut session, bad).expect("the field accepted it");

    assert_eq!(from_the_store, from_the_field);
    // Without this the assertion above passes when both sides fail for some
    // third reason — which is exactly what it did for as long as the id was
    // unquoted and the parser refused it before either declaration was read.
    assert!(from_the_store.contains("vector"), "{from_the_store}");
}

#[test]
fn a_record_with_no_vector_is_not_a_record_of_a_vector_store() {
    // What the word adds that the width alone does not. `TYPE vector<3>` leaves
    // the field optional — W11's own test says so in as many words — so this
    // refusal comes from the `REQUIRED` the declaration supplies.
    let held = store();
    let mut session = declared(&held);

    let said = refusal(
        &mut session,
        "CREATE embeddings:'empty' = { label: 'no vector here' };",
    )
    .expect("a record with no vector was accepted into a vector store");
    assert!(said.contains("vector"), "{said}");
}

#[test]
fn the_declaration_reads_back_as_the_word_that_made_it() {
    // The round trip `TableKind::Collection` was introduced for, one kind
    // further on: reported as `DEFINE TABLE … SCHEMALESS` a store re-executes
    // happily and comes back no longer knowing that its width, its index and
    // itself belong together.
    let held = store();
    let mut session = declared(&held);

    let report = reported(&mut session, "INFO FOR TABLE embeddings;");
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    let Some(Value::String(definition)) = fields.get("definition") else {
        panic!("no definition in {fields:?}");
    };
    assert!(
        definition.contains("DEFINE VECTOR embeddings DIMENSION 3 DISTANCE cosine"),
        "{definition}"
    );
    // And nothing else, because the word declares the field and the index: a
    // script that also wrote them would refuse on re-execution with the name
    // already taken.
    assert!(!definition.contains("DEFINE FIELD"), "{definition}");
    assert!(!definition.contains("DEFINE INDEX"), "{definition}");
}

#[test]
fn the_store_reports_its_width_its_distance_and_that_nobody_has_measured_it() {
    let held = store();
    let mut session = declared(&held);

    let report = reported(&mut session, "INFO FOR VECTOR embeddings;");
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    assert_eq!(fields.get("name"), Some(&Value::from("embeddings")));
    assert_eq!(fields.get("dimension"), Some(&Value::from(3)));
    assert_eq!(fields.get("distance"), Some(&Value::from("cosine")));
    // Absent, not zero. A recall of zero is a measurement saying the index finds
    // nothing; absence says nobody has asked, and reporting the second as the
    // first is a number nobody checked wearing the name of one somebody did.
    assert_eq!(fields.get("recall"), Some(&Value::None));
}

#[test]
fn a_distance_this_store_does_not_have_is_refused_by_name() {
    let held = store();
    let mut session = Session::new(&held);
    session.run(PLACE).unwrap();

    let said = refusal(
        &mut session,
        "DEFINE VECTOR embeddings DIMENSION 3 DISTANCE manhattan;",
    )
    .expect("an unknown distance was accepted");
    assert!(said.contains("manhattan"), "{said}");
    assert!(said.contains("cosine"), "{said}");
}

#[test]
fn dropping_a_vector_store_needs_the_word_that_made_it_to_name_a_vector_store() {
    // A `DROP VECTOR` that quietly removed an ordinary table would be a typo
    // with the blast radius of a table.
    let held = store();
    let mut session = declared(&held);
    session.run("DEFINE COLLECTION notes;").unwrap();

    let said = refusal(&mut session, "DROP VECTOR notes;")
        .expect("`DROP VECTOR` removed a table that is not a vector store");
    assert!(said.contains("notes"), "{said}");

    session.run("DROP VECTOR embeddings;").unwrap();
    // Gone, and the name is free again — which is the check that the field and
    // the index went with it rather than being left behind to collide.
    session
        .run("DEFINE VECTOR embeddings DIMENSION 8 DISTANCE euclidean;")
        .unwrap();
}

#[test]
fn a_width_the_declaration_could_not_write_back_is_refused_where_it_was_written() {
    let held = store();
    let mut session = Session::new(&held);
    session.run(PLACE).unwrap();

    let said = refusal(
        &mut session,
        "DEFINE VECTOR embeddings DIMENSION 70000 DISTANCE cosine;",
    )
    .expect("a width past the ceiling was accepted");
    assert!(said.contains("65536"), "{said}");

    let none = refusal(
        &mut session,
        "DEFINE VECTOR embeddings DIMENSION 0 DISTANCE cosine;",
    )
    .expect("a zero-wide store was accepted");
    assert!(none.contains("at least one"), "{none}");
}
