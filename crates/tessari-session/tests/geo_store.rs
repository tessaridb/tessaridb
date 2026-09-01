//! `DEFINE GEO` — a store of places, worked with the way a store is.
//!
//! # What the word adds over the three statements it stands for
//!
//! ```text
//! DEFINE COLLECTION places;
//! DEFINE FIELD geometry ON places TYPE geometry REQUIRED;
//! DEFINE INDEX geometry ON places FIELDS geometry SPATIAL;
//! ```
//!
//! Three statements that only work when all three are right. A geometry field
//! with no spatial index makes every place query a scan; a spatial index with no
//! declared field indexes nothing; neither without `REQUIRED` admits a record
//! with no geometry at all — legal in a table, and not a record a place store
//! can answer for. The word makes the three inseparable, and `INFO` answers with
//! the word rather than with the three.
//!
//! # The property this file exists to hold
//!
//! **One code path**, the same one [`vector_store`] holds. The store desugars
//! into those three statements through the same three functions the long
//! spellings use, so the test that matters is not that each refuses — it is that
//! the two refuse the *same write* with the *same words*, character for
//! character. Two implementations agree on the happy path and part company on
//! the day the data does not fit.
//!
//! # What is deliberately not here
//!
//! There is no test that a store declared for points refuses a polygon, because
//! the store declares no shape (Q-324). The read that needs a point refuses
//! everything else where that read happens, and a store narrowed to points could
//! not express a table of regions — so a region is a place here, and one of the
//! tests below says so.
//!
//! [`vector_store`]: ../vector_store/index.html

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
DEFINE DATABASE atlas; USE DATABASE atlas;
";

/// A point, written the way the language writes one.
const PARIS: &str = "geometry { type: 'Point', coordinates: [2.35, 48.85] }";

/// A session holding a geo store called `places`.
fn declared(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(&format!("{PLACE}DEFINE GEO places;")).unwrap();
    session
}

/// What a statement said when it was refused, or `None` when it was accepted.
fn refusal(session: &mut Session<'_>, statement: &str) -> Option<String> {
    session.run(statement).err().map(|error| error.to_string())
}

/// The one value a statement answered with.
fn reported(session: &mut Session<'_>, statement: &str) -> Value {
    match session.run(statement).unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_geo_store_takes_places_and_gives_them_back() {
    // The whole of what the word promises, in three statements: declare it,
    // write a place into it, read the place back.
    let held = store();
    let mut session = declared(&held);

    session
        .run(&format!(
            "CREATE places:'paris' = {{ geometry: {PARIS}, name: 'Paris' }};"
        ))
        .unwrap();

    let Outcome::Records { records, .. } =
        session.run("SELECT * FROM places;").unwrap().pop().unwrap()
    else {
        panic!("a select answered with something other than records");
    };
    assert_eq!(records.len(), 1, "{records:?}");
}

#[test]
fn the_store_and_the_field_refuse_the_same_write_with_the_same_words() {
    // The criterion, and the only test here that could not be written any other
    // way. Same record, same missing field — so the two messages are comparable
    // character for character, and any difference at all is the two doorways
    // having become two implementations.
    let bad = "CREATE places:'nowhere' = { name: 'nowhere' };";

    let one = store();
    let from_the_store = refusal(&mut declared(&one), bad).expect("the store accepted it");

    let two = store();
    let mut session = Session::new(&two);
    session
        .run(&format!(
            "{PLACE}DEFINE COLLECTION places;\n\
             DEFINE FIELD geometry ON places TYPE geometry REQUIRED;"
        ))
        .unwrap();
    let from_the_field = refusal(&mut session, bad).expect("the field accepted it");

    assert_eq!(from_the_store, from_the_field);
    // And the shared message is the one this test is about. Without this line
    // the assertion above passes when both sides fail for some third reason —
    // a mis-spelled record id refused by the parser before either declaration
    // is consulted compares equal to itself and proves nothing.
    assert!(from_the_store.contains("geometry"), "{from_the_store}");
}

#[test]
fn a_record_with_no_geometry_is_not_a_record_of_a_geo_store() {
    // What the word adds that the type alone does not: `TYPE geometry` leaves
    // the field optional, so this refusal comes from the `REQUIRED` the
    // declaration supplies.
    let held = store();
    let mut session = declared(&held);

    let said = refusal(
        &mut session,
        "CREATE places:'empty' = { name: 'nothing here' };",
    )
    .expect("a record with no geometry was accepted into a geo store");
    assert!(said.contains("geometry"), "{said}");
}

#[test]
fn a_region_is_a_place_because_the_store_narrows_no_shape() {
    // Q-324 as a test rather than as a paragraph. Narrowing the store to points
    // would have made this write a refusal, and this table inexpressible — while
    // `records_in_region` serves it correctly today.
    let held = store();
    let mut session = declared(&held);

    session
        .run(
            "CREATE places:'square' = { \
               geometry: geometry { type: 'Polygon', \
                 coordinates: [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]] }, \
               name: 'a square' };",
        )
        .unwrap();

    let Outcome::Records { records, .. } =
        session.run("SELECT * FROM places;").unwrap().pop().unwrap()
    else {
        panic!("a select answered with something other than records");
    };
    assert_eq!(records.len(), 1, "{records:?}");
}

#[test]
fn the_declaration_reads_back_as_the_word_that_made_it() {
    // Reported as `DEFINE TABLE … SCHEMALESS` a store re-executes happily and
    // comes back no longer knowing that its field, its index and itself belong
    // together.
    let held = store();
    let mut session = declared(&held);

    let report = reported(&mut session, "INFO FOR TABLE places;");
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    let Some(Value::String(definition)) = fields.get("definition") else {
        panic!("no definition in {fields:?}");
    };
    assert!(definition.contains("DEFINE GEO places"), "{definition}");
    // And nothing else, because the word declares the field and the index: a
    // script that also wrote them would refuse on re-execution with the name
    // already taken.
    assert!(!definition.contains("DEFINE FIELD"), "{definition}");
    assert!(!definition.contains("DEFINE INDEX"), "{definition}");
}

#[test]
fn the_store_reports_its_field_and_its_index_and_an_empty_store_has_no_measurement() {
    // Shorter than the vector store's report by the parameters a geo store does
    // not declare — but not by the measurement. `refinement` is `none` here
    // because nothing has been measured, and `none` is a different statement
    // from a ratio of zero.
    let held = store();
    let mut session = declared(&held);

    let report = reported(&mut session, "INFO FOR GEO places;");
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    assert_eq!(fields.get("name"), Some(&Value::from("places")));
    assert_eq!(fields.get("field"), Some(&Value::from("geometry")));
    assert_eq!(fields.get("index"), Some(&Value::from("geometry")));
    assert_eq!(fields.get("refinement"), Some(&Value::None));
    // The vector store's parameters are not here, and their absence is part of
    // what makes this report the geo one.
    assert_eq!(fields.get("recall"), None);
    assert_eq!(fields.get("dimension"), None);
}

/// The refinement report of `places`, or `None` when nothing was measured.
fn refinement(session: &mut Session<'_>) -> Option<std::collections::BTreeMap<String, Value>> {
    let report = reported(session, "INFO FOR GEO places;");
    let Value::Object(fields) = report else {
        panic!("{report:?}");
    };
    match fields.get("refinement") {
        Some(Value::Object(measured)) => Some(measured.clone()),
        Some(Value::None) => None,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_rebuild_measures_what_the_covering_offers_against_what_the_boxes_keep() {
    // C9's criterion, on a corpus small enough to work out by hand. A square
    // and a point inside it: each one's own box is a query the other's box
    // meets, so the covering offers one record per query and the box test keeps
    // it — two reached, two admitted, a ratio of exactly one hundred per cent.
    //
    // The asking record is excluded, which is why these are ones and not twos:
    // a record queried by its own box always finds itself.
    let held = store();
    let mut session = declared(&held);
    session
        .run(
            "CREATE places:'ward' = { geometry: geometry { type: 'Polygon', \
             coordinates: [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]] }, name: 'a ward' }; \
             CREATE places:'corner' = { geometry: geometry { type: 'Point', \
             coordinates: [1, 1] }, name: 'a corner' };",
        )
        .unwrap();

    // Records written after the build are placed one at a time, and a single
    // placement cannot see the whole index — so the figure comes from a rebuild,
    // which is the one statement that walks every row.
    assert_eq!(refinement(&mut session), None, "a write does not measure");

    session.run("REBUILD INDEX geometry ON places;").unwrap();
    let measured = refinement(&mut session).expect("a rebuild measures");

    assert_eq!(measured.get("sample"), Some(&Value::from(2_i64)));
    assert_eq!(measured.get("records"), Some(&Value::from(2_i64)));
    assert_eq!(measured.get("reached"), Some(&Value::from(2_i64)));
    assert_eq!(measured.get("admitted"), Some(&Value::from(2_i64)));
    // Reached over admitted, as a percentage. Nothing was offered and thrown
    // away, so the covering wasted none of the two reads it served.
    assert_eq!(measured.get("refinement"), Some(&Value::from(100_i64)));
    // Which query the figure answers for. The measurement asks the widest
    // relation there is, so a store only ever read with a narrower one refines
    // a smaller set at a cost this number does not describe — and without the
    // label there is nothing in the report to say so.
    assert_eq!(measured.get("relation"), Some(&Value::from("meets")));
}

#[test]
fn a_rebuild_with_nothing_to_measure_takes_the_previous_figure_with_it() {
    // The decisive test, and the only input on which the clear is observable.
    //
    // A rebuild overwrites the measurement key, so a test that measures, grows
    // the store and measures again passes with the clear removed — the second
    // write hides the missing delete. The clear is only visible on the path
    // where the rebuild writes NOTHING: measure first, then take the store below
    // what can be measured and rebuild. Without the clear the old figure stays,
    // describing a covering that no longer exists, and nothing fails until
    // somebody reads the number.
    let held = store();
    let mut session = declared(&held);
    session
        .run(
            "CREATE places:'ward' = { geometry: geometry { type: 'Polygon', \
             coordinates: [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]] }, name: 'a ward' }; \
             CREATE places:'corner' = { geometry: geometry { type: 'Point', \
             coordinates: [1, 1] }, name: 'a corner' }; \
             REBUILD INDEX geometry ON places;",
        )
        .unwrap();
    assert!(
        refinement(&mut session).is_some(),
        "the figure this test is about was never taken"
    );

    session
        .run("DELETE places:'corner'; REBUILD INDEX geometry ON places;")
        .unwrap();

    assert_eq!(
        refinement(&mut session),
        None,
        "a figure survived a rebuild that measured nothing"
    );
}

#[test]
fn dropping_a_geo_store_needs_the_word_that_made_it_to_name_a_geo_store() {
    // A `DROP GEO` that quietly removed an ordinary table would be a typo with
    // the blast radius of a table.
    let held = store();
    let mut session = declared(&held);
    session.run("DEFINE COLLECTION notes;").unwrap();

    let said = refusal(&mut session, "DROP GEO notes;").expect("DROP GEO removed a collection");
    assert!(said.contains("notes"), "{said}");

    // And the collection is still there to be found.
    session.run("INFO FOR TABLE notes;").unwrap();

    session.run("DROP GEO places;").unwrap();
    refusal(&mut session, "INFO FOR GEO places;").expect("the store outlived its own DROP");
}

#[test]
fn asking_a_table_that_is_not_a_store_for_its_store_report_is_refused() {
    // `INFO FOR GEO` names a kind, not a table. Answering for an ordinary table
    // would report a field and an index that mean something else.
    let held = store();
    let mut session = declared(&held);
    session.run("DEFINE COLLECTION notes;").unwrap();

    let said = refusal(&mut session, "INFO FOR GEO notes;").expect("a collection reported as one");
    assert!(said.contains("notes"), "{said}");
}
