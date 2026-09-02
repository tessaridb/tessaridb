//! `AFTER <record>` — a page that resumes from a record instead of an offset.
//!
//! # The failure
//!
//! `START 100000 LIMIT 20` reads a hundred thousand records to answer with
//! twenty, and it does it again, one record deeper, on every page. Worse, it is
//! not even *correct* under concurrent writes: a record inserted before the
//! cursor's position shifts every later page by one, so a walk that pages to the
//! end skips a record for every insert behind it and repeats one for every
//! delete.
//!
//! # What the tests here are actually pinning
//!
//! Not that the words parse. Four things, and each of them is a decision that a
//! passing parse would have hidden:
//!
//! - **the page is right**, including that the anchor itself is never in it and
//!   that a bound counts the page rather than the table;
//! - **the bound is applied after the cursor and not before it** — this is the
//!   one that would have shipped broken, because filtering a bounded answer
//!   leaves page two empty while filtering a bound's *input* leaves it correct;
//! - **the seek and the walk answer identically**, which is what makes the note
//!   a statement about cost and never about the records;
//! - **what the clause refuses**, at the point it can be refused: an offset
//!   beside it, a reshaping clause beside it, an anchor from another table, and
//!   an anchor that is gone on a read whose order needs the value it held.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION people;
CREATE people:1 = { name: 'ada', city: 'london', active: true };
CREATE people:2 = { name: 'bo', city: 'paris', active: true };
CREATE people:3 = { name: 'cy', city: 'london', active: false };
CREATE people:4 = { name: 'di', city: 'paris', active: true };
CREATE people:5 = { name: 'ed', city: 'london', active: true };
DEFINE COLLECTION notes;
CREATE notes:1 = { title: 'first' };
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

fn refuse(session: &mut Session<'_>, script: &str) -> String {
    session
        .run(script)
        .err()
        .unwrap_or_else(|| panic!("{script}: answered instead of refusing"))
        .to_string()
}

/// The identities the answer holds, in the order it holds them.
fn ids(answered: &Outcome) -> Vec<RecordId> {
    answered
        .records()
        .expect("records")
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

fn int(ids: &[RecordId]) -> Vec<i64> {
    ids.iter()
        .map(|id| match id {
            RecordId::Int(value) => *value,
            other => panic!("not an integer identity: {other:?}"),
        })
        .collect()
}

#[test]
fn a_page_begins_after_the_anchor_and_never_at_it() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM people AFTER people:2;");
    assert_eq!(
        int(&ids(&answered)),
        vec![3, 4, 5],
        "the anchor was in its own page, or the page did not start where it was asked to"
    );
}

#[test]
fn the_bound_counts_the_page_and_not_the_table() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM people AFTER people:2 LIMIT 2;");
    assert_eq!(int(&ids(&answered)), vec![3, 4]);
}

#[test]
fn paging_the_whole_table_visits_every_record_exactly_once() {
    let store = store();
    let mut session = ready(&store);
    let mut seen = Vec::new();
    let mut anchor = String::from("SELECT * FROM people LIMIT 2;");
    loop {
        let page = int(&ids(&run(&mut session, &anchor)));
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().copied());
        let last = *page.last().expect("a page that is not empty");
        anchor = format!("SELECT * FROM people AFTER people:{last} LIMIT 2;");
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_bounded_ordered_page_resumes_rather_than_answering_empty() {
    let store = store();
    let mut session = ready(&store);
    // The decision this whole clause turns on. The bound keeps the records the
    // order puts first; the cursor wants the ones after a position, which is the
    // other end. Applied to the bound's *output* the page comes back empty every
    // time — the right records, none of them, with nothing raised.
    let answered = run(
        &mut session,
        "SELECT * FROM people ORDER BY name AFTER people:2 LIMIT 2;",
    );
    assert_eq!(int(&ids(&answered)), vec![3, 4]);
}

#[test]
fn an_ordering_the_cursor_resumes_is_the_one_the_statement_named() {
    let store = store();
    let mut session = ready(&store);
    // Descending by name: after `di` come `cy`, `bo`, `ada`. Nothing about the
    // identities says so, which is the point — the cursor compares by the key.
    let answered = run(
        &mut session,
        "SELECT * FROM people ORDER BY name DESC AFTER people:4;",
    );
    assert_eq!(int(&ids(&answered)), vec![3, 2, 1]);
}

#[test]
fn a_cursor_over_a_condition_pages_the_records_the_condition_kept() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM people WHERE active = true AFTER people:2;",
    );
    assert_eq!(int(&ids(&answered)), vec![4, 5], "cy is not active");
}

#[test]
fn a_cursor_without_an_order_answers_in_the_stores_own_order() {
    let store = store();
    let mut session = ready(&store);
    // A filtered read may be served by an index, and an index's order is its
    // own. The clause supplies the order it resumes, so this answer is by
    // identity whatever path ran — otherwise a page depends on which indexes
    // happen to exist.
    let answered = run(
        &mut session,
        "SELECT * FROM people WHERE city = 'london' AFTER people:1;",
    );
    assert_eq!(int(&ids(&answered)), vec![3, 5]);
}

#[test]
fn the_seek_and_the_walk_answer_with_the_same_records() {
    let store = store();
    let mut session = ready(&store);
    // The seek runs on the plain table read; the walk runs on the same records
    // reached through a condition every one of them satisfies. A note that meant
    // anything about the answer rather than about the cost would show up here.
    let sought = run(&mut session, "SELECT * FROM people AFTER people:2;");
    let walked = run(
        &mut session,
        "SELECT * FROM people WHERE name != 'nobody' AFTER people:2;",
    );
    assert_eq!(int(&ids(&sought)), int(&ids(&walked)));
}

#[test]
fn a_sought_page_says_nothing_and_a_walked_one_says_it_walked() {
    let store = store();
    let mut session = ready(&store);
    let sought = run(&mut session, "SELECT * FROM people AFTER people:2;");
    assert!(
        !sought.notes().contains(&Note::CursorWalked),
        "a page that sought reported that it walked: {:?}",
        sought.notes()
    );
    let walked = run(
        &mut session,
        "SELECT * FROM people ORDER BY name AFTER people:2;",
    );
    assert!(
        walked.notes().contains(&Note::CursorWalked),
        "a page that walked said nothing: {:?}",
        walked.notes()
    );
}

#[test]
fn a_page_walk_survives_the_deletion_of_the_record_it_resumed_from() {
    let store = store();
    let mut session = ready(&store);
    run(&mut session, "DELETE people:2;");
    // The anchor names a position and a position is well defined whether or not
    // something sits on it. This is the ordinary end of a long walk otherwise:
    // the record the caller last saw is exactly the one most likely to be gone.
    let answered = run(&mut session, "SELECT * FROM people AFTER people:2;");
    assert_eq!(int(&ids(&answered)), vec![3, 4, 5]);
}

#[test]
fn an_ordered_page_refuses_an_anchor_that_is_gone() {
    let store = store();
    let mut session = ready(&store);
    run(&mut session, "DELETE people:2;");
    // Here the position is a value the record held, so with the record gone
    // there is nothing to resume from — and every guess picks a page.
    let refused = refuse(
        &mut session,
        "SELECT * FROM people ORDER BY name AFTER people:2;",
    );
    assert!(refused.contains("no longer holds"), "{refused}");
}

#[test]
fn an_offset_beside_a_cursor_is_refused_where_the_statement_is_read() {
    let store = store();
    let mut session = ready(&store);
    let refused = refuse(
        &mut session,
        "SELECT * FROM people AFTER people:2 START 1 LIMIT 2;",
    );
    assert!(
        refused.contains("both say where this page begins"),
        "{refused}"
    );
}

#[test]
fn an_anchor_from_another_table_is_refused_rather_than_compared() {
    let store = store();
    let mut session = ready(&store);
    // `notes:1` and `people:1` compare identically, so without this the page
    // would be real records — the wrong ones, quietly.
    let refused = refuse(&mut session, "SELECT * FROM people AFTER notes:1;");
    assert!(
        refused.contains("anchors this page in `notes`"),
        "{refused}"
    );
}

#[test]
fn a_clause_that_changes_what_a_row_is_refuses_a_cursor_beside_it() {
    let store = store();
    let mut session = ready(&store);
    for script in [
        "SELECT city, count(*) AS n FROM people GROUP BY city AFTER people:2;",
        "SELECT * FROM people FETCH city AFTER people:2;",
        "SELECT * FROM people SPLIT ON city AFTER people:2;",
    ] {
        let refused = refuse(&mut session, script);
        assert!(
            refused.contains("resumes after a record"),
            "{script}: {refused}"
        );
    }
}

#[test]
fn the_word_after_is_still_a_field_name() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "DEFINE COLLECTION runs; CREATE runs:1 = { after: 'yes' };",
    );
    let answered = run(&mut session, "SELECT after FROM runs;");
    let records = answered.records().expect("records");
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object")
    };
    assert_eq!(fields.get("after"), Some(&Value::from("yes")));
}

#[test]
fn a_cursor_past_the_last_record_answers_with_nothing() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM people AFTER people:5;");
    assert!(ids(&answered).is_empty());
}
