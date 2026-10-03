//! `OR` and `NOT` inside a search query, answered the same way with an index
//! and without one.
//!
//! # Why the tests are written as scan/index pairs rather than as feature checks
//!
//! `OR` returning more records and `NOT` returning fewer both pass on an
//! implementation that is quietly wrong, because more and fewer are what a
//! reader expects to see. What cannot pass is the two access paths disagreeing:
//! an inverted index answers a disjunction by unioning posting lists and a
//! conjunction by intersecting them, so the candidate set changes shape between
//! the forms while the predicate does not. That is the seam where an index
//! answers `[]` to a query the scan answers correctly — which is what happened
//! to the phrase operator one wave ago, and it was invisible until a test ran
//! both paths over the same fixture.
//!
//! So every test here runs `for indexed in [false, true]` and asserts the same
//! answer, and the fixture is built so that each form has at least one record
//! only it returns.
//!
//! # Why a bare negation is refused rather than answered
//!
//! An index enumerates **presence**. `NOT babbage` names the complement of a
//! posting list, which is every record the index cannot enumerate, so the only
//! plans are a full scan or a refusal. Refusing keeps the store's existing rule
//! — a statement that did not run beats one that quietly read the whole table —
//! and the refusal is raised before the catalog is read, so it cannot come to
//! depend on whether an index exists.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// Four records chosen so that no two of the forms answer with the same set.
///
/// `notes:1` holds `ada` and `babbage`; `notes:2` holds `ada` alone;
/// `notes:3` holds `lovelace` alone; `notes:4` holds neither name. So
/// `ada` is `{1,2}`, `ada OR lovelace` is `{1,2,3}`, and `ada NOT babbage` is
/// `{2}` — three different answers, which is what makes a test able to fail.
fn notes(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'ada worked with babbage on the engine' }};\n\
             CREATE notes:2 = {{ body: 'ada wrote the first program' }};\n\
             CREATE notes:3 = {{ body: 'lovelace is the surname' }};\n\
             CREATE notes:4 = {{ body: 'nothing to do with either of them' }};",
        ))
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    held
}

/// The record ids one read answered with, sorted so a set is compared as a set.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<String> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut found: Vec<String> = records.iter().map(|(id, _)| id.to_string()).collect();
    found.sort();
    found
}

/// The answer to one read on a store with the index and on one without it.
fn both_paths(read: &str) -> (Vec<String>, Vec<String>) {
    let mut answers = Vec::new();
    for indexed in [false, true] {
        let held = notes(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        answers.push(ids(&mut session, read));
    }
    (answers[0].clone(), answers[1].clone())
}

/// **The test that decides the wave.** One query using all three forms, answered
/// identically by the scan and by the index.
///
/// `ada OR lovelace NOT babbage` groups as `(ada | lovelace)` required and
/// `babbage` excluded, so it is `{1,2,3}` minus `{1}`. The index reaches that by
/// unioning two posting lists and then dropping what the condition refuses; the
/// scan reaches it by reading every record. Neither is allowed to be the reason
/// the answer changed.
#[test]
fn a_query_mixing_all_three_forms_answers_the_same_with_an_index_and_without() {
    let (scanned, indexed) = both_paths(
        "SELECT * FROM notes WHERE body MATCHES 'ada OR lovelace NOT babbage' ORDER BY id;",
    );

    assert_eq!(
        scanned, indexed,
        "the access path decides how the answer is found and never what it is"
    );
    assert_eq!(
        scanned,
        vec!["2".to_owned(), "3".to_owned()],
        "notes:2 holds ada without babbage and notes:3 holds lovelace; notes:1 \
         is excluded by babbage and notes:4 holds neither name"
    );
}

/// A disjunction returns the union, and the record holding **only** the second
/// term is the one that separates it from a conjunction.
#[test]
fn or_is_the_union_of_its_terms() {
    let (scanned, indexed) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'ada OR lovelace' ORDER BY id;");

    assert_eq!(scanned, indexed, "same answer on both access paths");
    assert_eq!(
        scanned,
        vec!["1".to_owned(), "2".to_owned(), "3".to_owned()],
        "notes:3 holds lovelace and no ada, so a conjunction would drop it"
    );
}

/// An exclusion removes a record the positive half returned, which is the only
/// way to see that it did anything.
#[test]
fn not_removes_a_record_the_required_terms_reached() {
    let (scanned, indexed) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'ada NOT babbage' ORDER BY id;");

    assert_eq!(scanned, indexed, "same answer on both access paths");
    assert_eq!(
        scanned,
        vec!["2".to_owned()],
        "ada alone is {{1,2}}; notes:1 also holds babbage and is excluded"
    );
}

/// The operators are recognised **as written**, so a query that predates them
/// still asks what it asked.
///
/// `or` in lower case is a word: the tokenizer keeps it, the stemmer maps it to
/// a term, and no record holds it — so the conjunction finds nothing. That is
/// the same answer this query gave before the operators existed, which is the
/// property being asserted.
#[test]
fn a_lowercase_or_is_a_word_and_not_an_operator() {
    let (scanned, indexed) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'ada or lovelace' ORDER BY id;");

    assert_eq!(scanned, indexed, "same answer on both access paths");
    assert!(
        scanned.is_empty(),
        "three words conjoined, and no record holds the literal word `or`"
    );
}

/// A query with no operator is still the conjunction it always was.
#[test]
fn a_plain_query_is_unchanged_by_the_boolean_forms() {
    let (scanned, indexed) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'ada babbage' ORDER BY id;");

    assert_eq!(scanned, indexed, "same answer on both access paths");
    assert_eq!(
        scanned,
        vec!["1".to_owned()],
        "only notes:1 holds both words"
    );
}

/// A bare negation is refused **by name**, and identically with an index and
/// without one.
///
/// The refusal is what makes the criterion honest: `EXPLAIN` cannot report
/// `via index` for a form the index cannot answer, so the language does not
/// offer that form rather than the planner quietly falling back to a scan.
#[test]
fn a_query_that_excludes_without_requiring_is_refused_by_name() {
    for indexed in [false, true] {
        let held = notes(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let refused = session
            .run("SELECT * FROM notes WHERE body MATCHES 'NOT babbage';")
            .expect_err("a bare negation has no index answer and is refused");

        let said = refused.to_string();
        assert!(
            said.contains("cannot exclude terms without requiring one"),
            "indexed={indexed}: the refusal names what is wrong with the query, \
             not what the store failed to do — got {said}"
        );
    }
}

/// The refusal is raised **before the catalog is read**, so it does not depend
/// on the field, the analyzer or the index existing.
///
/// Same shape as the malformed slop marker one wave ago: a mistake in the query
/// is not a question about the data, and a refusal that needed the schema would
/// make the same statement run on one table and fail on another.
#[test]
fn the_refusal_does_not_need_a_field_that_exists() {
    let held = store();
    let mut session = Session::new(&held);
    session.run(PLACE).unwrap();
    session.run("DEFINE COLLECTION notes;").unwrap();

    let refused = session
        .run("SELECT * FROM notes WHERE nothing MATCHES 'NOT babbage';")
        .expect_err("the query is refused whatever the field turns out to be");

    assert!(
        refused
            .to_string()
            .contains("cannot exclude terms without requiring one"),
        "the query is wrong on its own terms, so nothing about the schema \
         may change whether it is refused — got {refused}"
    );
}

/// `EXPLAIN` names the read the index actually performed.
///
/// A disjunction is a union per group intersected across groups, which is the
/// same read a prefix or fuzzy walk ends in — and reporting it under either of
/// their names would tell a reader that a dictionary walk ran when none did.
#[test]
fn explain_reports_a_disjunction_under_its_own_name() {
    let held = notes(true);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let outcomes = session
        .run("EXPLAIN SELECT * FROM notes WHERE body MATCHES 'ada OR lovelace';")
        .unwrap();
    let said = format!("{:?}", outcomes.last());

    assert!(
        said.contains("any-terms"),
        "a disjunction is served by its own shape, so the plan says which \
         question the index answered — got {said}"
    );
    assert!(
        !said.contains("prefix-terms") && !said.contains("fuzzy-terms"),
        "no dictionary walk ran, so no walk is reported — got {said}"
    );
}

/// A conjunction keeps reporting `terms`, so the new shape does not rename a
/// plan that has not changed.
#[test]
fn explain_still_reports_terms_for_a_query_with_no_operator() {
    let held = notes(true);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let outcomes = session
        .run("EXPLAIN SELECT * FROM notes WHERE body MATCHES 'ada babbage';")
        .unwrap();
    let said = format!("{:?}", outcomes.last());

    assert!(
        said.contains("terms") && !said.contains("any-terms"),
        "an unchanged query keeps its plan — got {said}"
    );
}

// ---- G051 T7.2: one query over several fields (S8), and an index kind that
// reads one field declared over several (S9) ----

/// `title` and `body` searched, `n` plain, each record answering a different
/// disjunct so a union that dropped either side, or a scan that read a third,
/// gives a different set.
fn titled(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION papers;\n\
             DEFINE FIELD title ON papers TYPE string ANALYZER english;\n\
             DEFINE FIELD body ON papers TYPE string ANALYZER english;\n\
             CREATE papers:1 = {{ title: 'ada on engines', body: 'notes', n: 1 }};\n\
             CREATE papers:2 = {{ title: 'a letter', body: 'lovelace wrote it', n: 2 }};\n\
             CREATE papers:3 = {{ title: 'ada and lovelace', body: 'both', n: 3 }};\n\
             CREATE papers:4 = {{ title: 'unrelated', body: 'nothing here', n: 4 }};",
        ))
        .unwrap();
    if indexed {
        session
            .run(
                "DEFINE INDEX by_title ON papers FIELDS title SEARCH;\n\
                 DEFINE INDEX by_body ON papers FIELDS body SEARCH;\n\
                 DEFINE INDEX by_n ON papers FIELDS n;",
            )
            .unwrap();
    }
    held
}

/// The answer to `read` without the indexes and with them, and the plan the
/// indexed store reports for it.
fn titled_both_ways(read: &str) -> (Vec<String>, Vec<String>, String) {
    let mut answers = Vec::new();
    let mut plan = String::new();
    for indexed in [false, true] {
        let held = titled(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        answers.push(ids(&mut session, read));
        if indexed {
            let explained = format!(
                "{:?}",
                session.run(&format!("EXPLAIN {read}")).unwrap().last()
            );
            // The plan the read itself reports, which must be the one EXPLAIN
            // says: two planners that agree today disagree the first time one
            // changes.
            let Some(Outcome::Records { plan: taken, .. }) = session.run(read).unwrap().pop()
            else {
                panic!("{read}");
            };
            assert_eq!(
                explained.contains("union"),
                taken.shape == Some("union"),
                "{explained} against {taken:?}"
            );
            plan = explained;
        }
    }
    (answers[0].clone(), answers[1].clone(), plan)
}

#[test]
fn a_disjunction_across_two_searched_fields_is_served_by_both_indexes() {
    let read = "SELECT * FROM papers WHERE title MATCHES 'ada' OR body MATCHES 'lovelace';";
    let (scanned, indexed, plan) = titled_both_ways(read);
    assert_eq!(scanned, indexed);
    assert_eq!(scanned, ["1", "2", "3"]);
    assert!(
        plan.contains("union") && plan.contains("by_title") && plan.contains("by_body"),
        "{plan}"
    );
    assert!(!plan.contains("String(\"scan\")"), "{plan}");
}

#[test]
fn a_disjunction_of_a_search_and_a_value_is_one_union_too() {
    let read = "SELECT * FROM papers WHERE title MATCHES 'lovelace' OR n = 1 OR n = 4;";
    let (scanned, indexed, plan) = titled_both_ways(read);
    assert_eq!(scanned, indexed);
    assert_eq!(scanned, ["1", "3", "4"]);
    assert!(plan.contains("union"), "{plan}");
}

#[test]
fn a_disjunct_no_index_serves_leaves_the_read_a_scan() {
    // `body` is searched, `n > 1` has no ordered index here to serve it as a
    // range of its own — and one side unserved means the union cannot be
    // complete, so the read scans and answers the same.
    let read = "SELECT * FROM papers WHERE body MATCHES 'lovelace' OR title = 'unrelated';";
    let (scanned, indexed, plan) = titled_both_ways(read);
    assert_eq!(scanned, indexed);
    assert_eq!(scanned, ["2", "4"]);
    assert!(
        plan.contains("String(\"scan\")") && !plan.contains("union"),
        "{plan}"
    );
}

#[test]
fn an_index_kind_that_reads_one_field_is_refused_over_several() {
    let held = titled(false);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();
    for statement in [
        "DEFINE INDEX by_both ON papers FIELDS title, body SEARCH;",
        "DEFINE INDEX by_both ON papers FIELDS title, body SPATIAL;",
        "DEFINE INDEX by_both ON papers FIELDS title, body VECTOR cosine;",
    ] {
        match session.run(statement) {
            Err(tessari_session::Error::Store(tessari_storage::Error::IndexReadsOneField {
                name,
                fields,
                ..
            })) => {
                assert_eq!((name.as_str(), fields), ("by_both", 2), "{statement}");
            }
            other => panic!("{statement}: {other:?}"),
        }
    }
    // The control: an ordinary index over two fields is a composite key.
    session
        .run("DEFINE INDEX by_both ON papers FIELDS title, body;")
        .unwrap();
}

/// G051 T7.2 (inventory S26): a space whose values are objects is searched
/// through a field's index like a table; a scalar value has no field to name
/// and is not (Q-866).
#[test]
fn a_space_holding_objects_is_searched_through_its_index() {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE SPACE kv;\n\
             DEFINE FIELD body ON kv TYPE string ANALYZER english;\n\
             DEFINE INDEX by_body ON kv FIELDS body SEARCH;\n\
             SET kv:'a' = {{ body: 'the quick foxes' }};\n\
             SET kv:'b' = {{ body: 'a lazy dog' }};\n\
             SET kv:'c' = 'a fox, but not in a field';",
        ))
        .unwrap();
    let read = "SELECT * FROM kv WHERE body MATCHES 'fox';";
    assert_eq!(ids(&mut session, read), ["a"]);
    let plan = format!(
        "{:?}",
        session.run(&format!("EXPLAIN {read}")).unwrap().last()
    );
    assert!(plan.contains("by_body") && plan.contains("terms"), "{plan}");
}

/// A space whose values are text: the route `value` names the value itself,
/// so it is declared, indexed and searched like a field — and the scan and the
/// index answer alike. An object keeps its own `value` field.
#[test]
fn a_space_of_scalar_text_is_searched_through_its_value() {
    let mut answers = Vec::new();
    for indexed in [false, true] {
        let held = store();
        let mut session = Session::new(&held);
        let index = if indexed {
            "DEFINE INDEX by_value ON kv FIELDS value SEARCH;\n"
        } else {
            ""
        };
        session
            .run(&format!(
                "{PLACE}\
                 DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
                 DEFINE SPACE kv;\n\
                 DEFINE FIELD value ON kv TYPE string ANALYZER english;\n\
                 {index}\
                 SET kv:'a' = 'the quick foxes';\n\
                 SET kv:'b' = 'a lazy dog';\n\
                 SET kv:'c' = {{ value: 'a fox in a field named value' }};",
            ))
            .unwrap();
        let read = "SELECT * FROM kv WHERE value MATCHES 'fox';";
        answers.push(ids(&mut session, read));
        assert_eq!(answers.last().unwrap(), &["a", "c"], "indexed: {indexed}");
        if indexed {
            let plan = format!(
                "{:?}",
                session.run(&format!("EXPLAIN {read}")).unwrap().last()
            );
            assert!(
                plan.contains("by_value") && plan.contains("terms"),
                "{plan}"
            );
            let projected = session.run("SELECT value FROM kv:'a';").unwrap();
            assert!(
                format!("{:?}", projected.last()).contains("the quick foxes"),
                "{projected:?}"
            );
        }
    }
    assert_eq!(answers[0], answers[1]);
}

/// Chinese and Japanese text is searchable: each ideograph is a token, so a
/// quoted run of them is an exact substring and the index and the scan agree.
/// An unquoted run asks for every character anywhere, as any unquoted query
/// asks for every word.
#[test]
fn a_quoted_run_of_ideographs_is_found_as_written() {
    for indexed in [false, true] {
        let held = store();
        let mut session = Session::new(&held);
        let index = if indexed {
            "DEFINE INDEX by_body ON notes FIELDS body SEARCH POSITIONS;\n"
        } else {
            ""
        };
        session
            .run(&format!(
                "{PLACE}\
                 DEFINE ANALYZER plain FILTERS lowercase;\n\
                 DEFINE COLLECTION notes;\n\
                 DEFINE FIELD body ON notes TYPE string ANALYZER plain;\n\
                 {index}\
                 CREATE notes:'a' = {{ body: '東京都に住む' }};\n\
                 CREATE notes:'b' = {{ body: '京東の店' }};\n\
                 CREATE notes:'c' = {{ body: 'Tokyo only' }};",
            ))
            .unwrap();
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM notes WHERE body MATCHES '\"東京\"';"
            ),
            ["a"],
            "indexed: {indexed}"
        );
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM notes WHERE body MATCHES '東京';"
            ),
            ["a", "b"],
            "indexed: {indexed}"
        );
    }
}
