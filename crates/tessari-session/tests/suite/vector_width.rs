//! A field that declares how wide its vectors are, and refuses the rest.
//!
//! # The failure this closes
//!
//! Nothing declared a width before this. A 512-wide row and a 768-wide row sat
//! together in one field legally, and the only thing that ever noticed was the
//! distance function — per read, long after the bad write, at the point furthest
//! from the cause. It did not notice loudly either: a vector of the wrong shape
//! is *infinitely far* from everything, which is a plausible ordering rather than
//! a complaint. So the wrong answer looked exactly like a right one.
//!
//! # Why the width is on the field
//!
//! Because that is where a vector is: an array of numbers in an ordinary field,
//! with the index built over it. A table may hold two of them — the last test
//! here writes one — and a width declared for the *table* could only govern one
//! and would leave the other as unchecked as it was.
//!
//! # Why both doorways get a test each
//!
//! `DEFINE FIELD … TYPE` and the column list of `DEFINE TABLE` are two ways to
//! say one thing. They go through one parser function today, so they cannot
//! disagree — and this file asserts that they still cannot, because "one
//! function" is a property of the code that a later change can quietly end.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE library; USE DATABASE library;
";

/// A session whose `documents` table declares `embedding` through `DEFINE FIELD`.
fn through_the_field(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE COLLECTION documents;\n\
             DEFINE FIELD embedding ON documents TYPE vector<3>;"
        ))
        .unwrap();
    session
}

/// The same declaration, written in the table's column list.
fn through_the_table(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE TABLE documents (embedding vector<3>);"
        ))
        .unwrap();
    session
}

/// What a write said when it was refused, or `None` when it was accepted.
fn refusal(session: &mut Session<'_>, statement: &str) -> Option<String> {
    session.run(statement).err().map(|error| error.to_string())
}

#[test]
fn a_declared_width_refuses_a_write_of_any_other_width() {
    let held = store();
    let mut session = through_the_field(&held);

    // The right width goes in.
    session
        .run("CREATE documents:1 = { embedding: [1.0, 2.0, 3.0] };")
        .unwrap();

    // Too short and too long are both refused, and the message carries the
    // width on **both** sides: without it a reader is told their array is an
    // array, which they knew.
    let short = refusal(
        &mut session,
        "CREATE documents:2 = { embedding: [1.0, 2.0] };",
    )
    .expect("a two-wide vector was accepted into a vector<3> field");
    assert!(short.contains("vector<3>"), "{short}");
    assert!(short.contains("vector<2>"), "{short}");

    let long = refusal(
        &mut session,
        "CREATE documents:3 = { embedding: [1.0, 2.0, 3.0, 4.0] };",
    )
    .expect("a four-wide vector was accepted into a vector<3> field");
    assert!(long.contains("vector<4>"), "{long}");
}

#[test]
fn the_two_doorways_refuse_the_same_write_with_the_same_words() {
    // The one test C5 is actually about. Same table name, same record, same
    // value — so the two messages are comparable character for character, and
    // any difference at all is the two doorways disagreeing.
    let bad = "CREATE documents:2 = { embedding: [1.0, 2.0] };";

    let one = store();
    let from_field = refusal(&mut through_the_field(&one), bad).expect("field doorway accepted it");

    let two = store();
    let from_table = refusal(&mut through_the_table(&two), bad).expect("table doorway accepted it");

    assert_eq!(from_field, from_table);
}

#[test]
fn a_width_refuses_what_is_not_a_vector_at_all_by_its_own_name() {
    let held = store();
    let mut session = through_the_field(&held);

    // Not an array, and an array that is not numbers. Neither has a width, so
    // neither is described as having one — inventing `vector<3>` for a list of
    // strings would name a shape the value does not have.
    for (statement, expected) in [
        ("CREATE documents:4 = { embedding: 'three' };", "string"),
        (
            "CREATE documents:5 = { embedding: ['a', 'b', 'c'] };",
            "array",
        ),
        ("CREATE documents:6 = { embedding: [] };", "array"),
    ] {
        let said = refusal(&mut session, statement).expect(statement);
        assert!(said.contains(expected), "{statement}\n  said: {said}");
        assert!(said.contains("vector<3>"), "{statement}\n  said: {said}");
    }
}

#[test]
fn the_width_reads_back_out_of_the_catalog_as_it_was_written() {
    // The catalog stores a kind as its spelling, so a width that writes one way
    // and reads another is a declaration that changes meaning when the store
    // reopens — and every write after that is checked against something nobody
    // typed. `INFO FOR TABLE` is where a reader goes to find out what a column
    // holds, so it is where the round trip is visible.
    let held = store();
    let mut session = through_the_field(&held);

    let reported = format!("{:?}", session.run("INFO FOR TABLE documents;").unwrap());
    assert!(reported.contains("vector<3>"), "{reported}");
}

#[test]
fn a_declared_width_does_not_make_the_field_mandatory() {
    // The rule every kind already follows, and a width does not change it:
    // a record missing the field is not indexed rather than refused, and
    // `REQUIRED` is the separate constraint that says otherwise.
    let held = store();
    let mut session = through_the_field(&held);

    session
        .run("CREATE documents:7 = { title: 'no vector here' };")
        .unwrap();
    session
        .run("CREATE documents:8 = { embedding: NULL };")
        .unwrap();
}

#[test]
fn one_table_holds_two_vectors_of_different_widths_and_checks_each() {
    // The argument the whole shape rests on. A title embedding and a body
    // embedding, of different widths, on one table — ordinary, and impossible
    // to govern with a width declared for the table.
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}DEFINE TABLE documents (title_at vector<2>, body_at vector<4>);"
        ))
        .unwrap();

    session
        .run("CREATE documents:1 = { title_at: [1.0, 2.0], body_at: [1.0, 2.0, 3.0, 4.0] };")
        .unwrap();

    // Each field is held to its own width, including the case that would pass
    // if one width governed the table: the body's value in the title's field.
    let swapped = refusal(
        &mut session,
        "CREATE documents:2 = { title_at: [1.0, 2.0, 3.0, 4.0] };",
    )
    .expect("a four-wide vector was accepted into a vector<2> field");
    assert!(swapped.contains("vector<2>"), "{swapped}");
    assert!(swapped.contains("vector<4>"), "{swapped}");
}
