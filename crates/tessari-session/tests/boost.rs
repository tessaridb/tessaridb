//! Weighting one field above another, and the property that keeps a weight
//! honest.
//!
//! # A boost is arithmetic, and that is the design rather than a shortcut
//!
//! A score is an ordinary expression, so weighting a field is multiplying it:
//! `search::score(title, q) * 3 + search::score(body, q)`. No syntax was added
//! for this, because none was missing — and a dedicated `^3` would have to
//! define its own precedence, its own interaction with the rest of an `ORDER BY`
//! expression, and its own answer to what a boost on an unindexed field means,
//! all of which arithmetic already answers.
//!
//! What *is* new is that this file proves the multiplication is safe to write,
//! which was not obvious and is the reason the wave carries a test rather than
//! a paragraph.
//!
//! # The property being protected
//!
//! Measured one wave earlier: giving a term the record does not hold any weight
//! at all does not tie the ranking, it **inverts** it — BM25 divides by document
//! length, so the shortest document wins a term nobody holds. The same trap sits
//! under a boost. A boosted field must multiply a score the record actually
//! earned; if a record matching nothing in `title` were scored as anything other
//! than exactly `0`, a large boost would promote precisely the records that
//! matched least, and the resulting order would look like a strong opinion
//! rather than a bug.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

/// Two searchable fields, and records that match the word in one, the other, or
/// neither.
///
/// `notes:1` holds `lock` in the title only, `notes:2` in the body only, and
/// `notes:3` in neither. Without a boost the first two are ordered by BM25
/// alone; with one they are not, and `notes:3` is the record a manufactured
/// score would lift.
fn notes() -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD title ON notes TYPE string ANALYZER english;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             DEFINE INDEX by_title ON notes FIELDS title SEARCH;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             CREATE notes:1 = {{ title: 'lock', body: 'a note about waiting' }};\n\
             CREATE notes:2 = {{ title: 'waiting', body: 'a note about lock' }};\n\
             CREATE notes:3 = {{ title: 'unrelated', body: 'nothing of the kind' }};",
        ))
        .unwrap();
    held
}

/// The record ids one read answered with, in the order returned.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<String> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records.iter().map(|(id, _)| id.to_string()).collect()
}

/// The number one read answered with under `relevance`, per record.
fn scores(session: &mut Session<'_>, read: &str) -> Vec<(String, f64)> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(id, payload)| {
            let Value::Object(object) = payload else {
                panic!("a record is an object: {payload:?}");
            };
            let held = object
                .get("relevance")
                .unwrap_or_else(|| panic!("a ranked read answers with its score"));
            let Value::Number(Number::Float(score)) = held else {
                panic!("a score is a float: {held:?}");
            };
            (id.to_string(), *score)
        })
        .collect()
}

/// **The test the boost exists for.** Weighting the title changes which record
/// comes first.
///
/// Both records match the word exactly once, in fields of similar length, so the
/// unweighted sum separates them barely if at all. Multiplying the title term
/// decides it — which is the whole claim "the language can say a per-field
/// boost" makes.
#[test]
fn weighting_a_field_changes_the_order() {
    let held = notes();
    let mut session = Session::new(&held);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();

    let boosted = ids(
        &mut session,
        "SELECT id, search::score(title, 'lock') * 10 + search::score(body, 'lock') \
             AS relevance \
           FROM notes ORDER BY relevance DESC;",
    );

    assert_eq!(
        boosted[0], "1",
        "notes:1 matches in the title, which was weighted ten times the body — \
         got {boosted:?}"
    );
}

/// **The property that keeps a boost honest.** A field the record does not match
/// contributes exactly `0`, so a boost multiplies a score that was earned.
///
/// Asserted as a number rather than as an order, because an order can be right
/// for the wrong reason. `notes:3` matches neither field, so every term in the
/// weighted sum is `0 × n`; if the unmatched field scored anything at all, the
/// multiplier would amplify it and the shortest record would arrive first — the
/// exact inversion measured one wave ago.
#[test]
fn a_boost_multiplies_a_score_the_record_earned_and_never_manufactures_one() {
    let held = notes();
    let mut session = Session::new(&held);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();

    let ranked = scores(
        &mut session,
        "SELECT id, search::score(title, 'lock') * 1000 + \
                    search::score(body, 'lock') * 1000 AS relevance \
           FROM notes ORDER BY relevance DESC;",
    );

    let unmatched = ranked
        .iter()
        .find(|(id, _)| id == "3")
        .expect("notes:3 is answered, scoring zero rather than being dropped");
    assert!(
        unmatched.1.abs() < f64::EPSILON,
        "a record matching neither field scores exactly zero however large the \
         weights are — got {unmatched:?}"
    );
    assert_eq!(
        ranked.last().map(|(id, _)| id.as_str()),
        Some("3"),
        "and it therefore sorts last, rather than being lifted by the weight — \
         got {ranked:?}"
    );
}

/// The weight is the only difference between two reads, so it has to be able to
/// reverse them.
///
/// Boosting the *body* instead puts the other record first. One assertion in
/// each direction, because a test that only ever boosts one field passes on an
/// implementation that ignores the multiplier and happens to agree with it.
#[test]
fn boosting_the_other_field_reverses_the_answer() {
    let held = notes();
    let mut session = Session::new(&held);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();

    let boosted = ids(
        &mut session,
        "SELECT id, search::score(title, 'lock') + search::score(body, 'lock') * 10 \
             AS relevance \
           FROM notes ORDER BY relevance DESC;",
    );

    assert_eq!(
        boosted[0], "2",
        "notes:2 matches in the body, which is the field weighted this time — \
         got {boosted:?}"
    );
}
