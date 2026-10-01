//! `MATCHES PREFIX` — the words a reader has started typing.
//!
//! # Two paths, one answer
//!
//! Every assertion here runs twice: once on a table with a search index and once
//! on the same data without one. That is not thoroughness, it is the store's
//! central rule — *which access path runs is decided by what exists; the answer
//! is not* — and a prefix operator is where that rule is easiest to break,
//! because the two paths reach the words by completely different means. The scan
//! analyses each record and compares; the index walks a dictionary of distinct
//! terms and unions their posting lists.
//!
//! # The contract, and which half of it is a refusal
//!
//! **The minimum length is a refusal** and it is raised before any access path
//! is chosen, so adding an index can never change whether a statement runs.
//!
//! **The expansion cap is not.** A prefix reaching more terms than the index
//! will union is answered by the scan instead, and `EXPLAIN` says `scan`. A cap
//! that refused would make a query succeed on a table without an index and fail
//! on the same table once somebody added one — the failure the rule above exists
//! to prevent, arriving from the direction nobody watches.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The same rows, the same analyzer, and an index only when asked for.
///
/// The analyzer stems, deliberately: a prefix over a stemmed field is the case
/// with a property worth pinning, and an unstemmed fixture would never reach it.
fn peopled(indexed: bool) -> Store {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = { body: 'Vector search over a store' };\n\
             CREATE notes:2 = { body: 'Vectors and their distances' };\n\
             CREATE notes:3 = { body: 'Locking and contention' };\n\
             CREATE notes:4 = { body: 'A vectorised inner loop' };\n\
             CREATE notes:5 = { body: 'Running a compaction' };",
        )
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    store
}

/// The ids a read answered with, sorted so two paths compare as sets, and the
/// path it took.
fn answered(session: &mut Session<'_>, read: &str) -> (Vec<String>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut found: Vec<String> = records.iter().map(|(id, _)| id.to_string()).collect();
    found.sort();
    (found, plan.access)
}

/// Run one statement against both fixtures and assert they agree.
///
/// The agreement is the assertion. What the two answered is returned so a caller
/// can then say what it should have been — a test that only checked agreement
/// would pass on two paths that are wrong in the same way, which for these two
/// is not a far-fetched failure: they share the analyzer.
fn both(statement: &str) -> Vec<String> {
    let indexed = peopled(true);
    let scanned = peopled(false);
    let mut with = Session::new(&indexed);
    let mut without = Session::new(&scanned);
    with.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    without
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let (left, path) = answered(&mut with, statement);
    let (right, _) = answered(&mut without, statement);
    assert_eq!(
        left, right,
        "the index and the scan disagreed on {statement:?} (indexed path {path:?})"
    );
    left
}

#[test]
fn a_prefix_reaches_every_word_that_begins_with_it() {
    // `vector`, `vectors` and `vectorised` are three terms and one prefix. The
    // records holding them are the answer; the one about locking is not.
    let found = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'vecto';");
    assert_eq!(found, vec!["1".to_owned(), "2".to_owned(), "4".to_owned()]);
}

#[test]
fn a_prefix_is_still_a_conjunction_across_the_words_that_were_typed() {
    // Every prefix must be matched by some word, which is what `MATCHES` means
    // one level looser. Only the record holding both survives.
    let found = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'vecto sea';");
    assert_eq!(found, vec!["1".to_owned()]);

    // And a prefix nothing begins with takes the whole conjunction with it.
    let none = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'vecto zzz';");
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn a_complete_word_is_a_prefix_of_itself() {
    // So `MATCHES PREFIX` never answers with less than `MATCHES` would, which is
    // the property that makes it safe to offer a reader as they type.
    //
    // It is also the case that broke when this operator was first written, and
    // it broke in the direction nobody would test: `contention` is stored as
    // `content`, so the reader who typed six letters found the record and the
    // reader who typed all ten did not. `Analyzer::prefixes` carries the fix and
    // the argument.
    let prefix = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'contention';");
    let exact = both("SELECT id FROM notes WHERE body MATCHES 'contention';");
    assert_eq!(prefix, exact);
    assert_eq!(prefix, vec!["3".to_owned()]);
}

#[test]
fn a_prefix_over_a_stemmed_field_is_a_prefix_of_the_stem() {
    // The property `Analyzer::prefixes` documents, pinned where a reader would
    // meet it. `Running` is stored as `run`, so `run` reaches it...
    let found = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'run';");
    assert_eq!(found, vec!["5".to_owned()]);

    // ...and `runni` does not, because the store never held those letters. That
    // is not a bug to be fixed by stemming the prefix — stemming `runni` gives
    // `runni`, which is the beginning of nothing.
    let longer = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'runni';");
    assert!(longer.is_empty(), "{longer:?}");
}

#[test]
fn a_prefix_shorter_than_the_contract_is_refused_and_the_refusal_names_the_limit() {
    for statement in [
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'v';",
        "SELECT id FROM notes WHERE body MATCHES PREFIX 've';",
        // The refusal is about the shortest word, not about the query's total
        // length: one usable prefix does not license an unusable one beside it.
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'vector a';",
    ] {
        for indexed in [true, false] {
            let store = peopled(indexed);
            let mut session = Session::new(&store);
            session
                .run("USE NAMESPACE prod; USE DATABASE shop;")
                .unwrap();
            let error = session.run(statement).unwrap_err();
            assert!(
                matches!(error, Error::PrefixTooShort { .. }),
                "indexed={indexed} {statement:?}: {error:?}"
            );
            let text = error.to_string();
            assert!(
                text.contains('3'),
                "the refusal does not state the limit: {text}"
            );
        }
    }
}

#[test]
fn a_prefix_at_the_minimum_is_served_rather_than_refused() {
    // The other boundary. A contract tested only from the refusing side is a
    // contract that would pass with the limit set to anything larger.
    let found = both("SELECT id FROM notes WHERE body MATCHES PREFIX 'vec';");
    assert_eq!(found, vec!["1".to_owned(), "2".to_owned(), "4".to_owned()]);
}

#[test]
fn a_prefix_past_the_expansion_cap_is_answered_by_the_scan_and_not_refused() {
    // The half of the contract that is deliberately **not** a refusal. Sixty-five
    // words share `wide`, which is one past the cap, so the index declines the
    // candidate and the scan answers — with the same records, which is the
    // assertion that matters.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER plain FILTERS lowercase;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER plain;",
        )
        .unwrap();
    for n in 0..65_u32 {
        session
            .run(&format!(
                "CREATE notes:{n} = {{ body: 'wide{n:04} common' }};"
            ))
            .unwrap();
    }
    // A narrow prefix reaching two of them, to prove the cap is what changed the
    // path and not the fixture.
    session
        .run("CREATE notes:900 = { body: 'narrowly common' };")
        .unwrap();

    let (before, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'wide';",
    );
    assert_eq!(path, AccessPath::Scan, "no index yet, so nothing else is");
    assert_eq!(before.len(), 65);

    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    let (after, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'wide';",
    );
    assert_eq!(
        after, before,
        "adding an index changed the answer at the cap"
    );
    assert_eq!(
        path,
        AccessPath::Scan,
        "the index served an expansion past its cap"
    );

    // And a prefix inside the cap is served by the index on the same table, so
    // the fallback is about the expansion rather than about the index being
    // unusable.
    let (found, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'narrow';",
    );
    assert_eq!(path, AccessPath::Index);
    assert_eq!(found, vec!["900".to_owned()]);
}

#[test]
fn the_index_serves_a_prefix_and_the_scan_serves_it_when_there_is_none() {
    // Both halves, because a test that only asserted the index path would pass
    // on an implementation that never used it — the answers would still be
    // right, and the walk would be a scan wearing an index's name.
    let indexed = peopled(true);
    let mut session = Session::new(&indexed);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let (found, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'vecto';",
    );
    assert_eq!(path, AccessPath::Index, "the index did not serve it");
    assert_eq!(found.len(), 3);

    let scanned = peopled(false);
    let mut session = Session::new(&scanned);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let (found, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES PREFIX 'vecto';",
    );
    assert_eq!(path, AccessPath::Scan, "a table with no index used one");
    assert_eq!(found.len(), 3);
}

/// A trailing `*` makes one word of a `MATCHES` query a prefix (ADR-0104), and
/// the rest of the query keeps its meaning: whole words, `OR`, `NOT`. Every
/// form is asked of both fixtures, and then pinned to what it should be.
#[test]
fn a_starred_word_in_matches_is_a_prefix_on_both_paths() {
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES 'vecto*';"),
        vec!["1".to_owned(), "2".to_owned(), "4".to_owned()]
    );
    // A whole word beside it is still a whole word.
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES 'search vecto*';"),
        vec!["1".to_owned()]
    );
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES 'vecto* OR lock*';"),
        vec![
            "1".to_owned(),
            "2".to_owned(),
            "3".to_owned(),
            "4".to_owned()
        ]
    );
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES 'vecto* NOT search';"),
        vec!["2".to_owned(), "4".to_owned()]
    );
    // The control: the same letters without the star are a word nobody wrote.
    assert!(both("SELECT id FROM notes WHERE body MATCHES 'vecto';").is_empty());
}

#[test]
fn a_starred_word_is_served_by_the_index() {
    let indexed = peopled(true);
    let mut session = Session::new(&indexed);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let (_, path) = answered(
        &mut session,
        "SELECT id FROM notes WHERE body MATCHES 'search vecto*';",
    );
    assert_ne!(
        path,
        AccessPath::Scan,
        "a starred word fell back to the scan"
    );
}

#[test]
fn a_starred_word_shorter_than_three_letters_is_refused_on_both_paths() {
    for indexed in [false, true] {
        let held = peopled(indexed);
        let mut session = Session::new(&held);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        let refused = session.run("SELECT id FROM notes WHERE body MATCHES 'search ve*';");
        assert!(
            matches!(refused, Err(Error::PrefixTooShort { ref prefix, .. }) if prefix == "ve"),
            "indexed {indexed}: {refused:?}"
        );
    }
}

/// Words that share a beginning, one of them rarer: `vectorz` is held by two
/// records, `vector` by three. Every record is three tokens long. `notes:6`
/// holds only `vectorz`, three times, so it leads — and a walk that pruned by
/// whole-term bounds would abandon `vectorz`'s postings and lose it.
fn blended() -> Store {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             CREATE notes:1 = { body: 'vector vector alpha' };\n\
             CREATE notes:2 = { body: 'vector beta gamma' };\n\
             CREATE notes:3 = { body: 'vector delta zeta' };\n\
             CREATE notes:4 = { body: 'vectorz eta theta' };\n\
             CREATE notes:5 = { body: 'nothing here either' };\n\
             CREATE notes:6 = { body: 'vectorz vectorz vectorz' };",
        )
        .unwrap();
    store
}

fn ranked(session: &mut Session<'_>, read: &str) -> Vec<(String, f64)> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(id, value)| {
            let tessari_types::Value::Object(fields) = value else {
                panic!("{value:?}");
            };
            let Some(tessari_types::Value::Number(score)) = fields.get("score") else {
                panic!("{fields:?}");
            };
            (id.to_string(), score.as_float().unwrap())
        })
        .collect()
}

/// A starred word is scored as **one** term (ADR-0104 D4): every expansion
/// shares the largest document frequency among them, and a record's frequency
/// is the sum of its occurrences. So the records using the word most lead, the
/// rest tie and fall to identity order — and the record holding the rarer
/// `vectorz` does **not** jump the queue on its rarity, which is what scoring
/// each expansion by its own frequency would do.
#[test]
fn a_starred_word_is_scored_as_one_blended_term() {
    let held = blended();
    let mut session = Session::new(&held);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    // The last form has no `WHERE`: it is the read the pruned top-k walk serves.
    for (filter, limit) in [
        ("WHERE body MATCHES 'vecto*' ", ""),
        ("WHERE body MATCHES 'vecto*' ", " LIMIT 2"),
        ("", " LIMIT 2"),
    ] {
        let found = ranked(
            &mut session,
            &format!(
                "SELECT id, search::score(body, 'vecto*') AS score FROM notes \
                 {filter}ORDER BY search::score(body, 'vecto*') DESC{limit};"
            ),
        );
        let order: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        let expected: &[&str] = if limit.is_empty() {
            &["6", "1", "2", "3", "4"]
        } else {
            &["6", "1"]
        };
        assert_eq!(order, expected, "{filter}{limit}");
        assert!(found[0].1 > found[1].1, "{found:?}");
        assert!(found[1].1 > 0.0, "a prefix scored nothing: {found:?}");
        if limit.is_empty() {
            assert!(
                (found[2].1 - found[4].1).abs() < 1e-12,
                "the rare expansion was weighed on its own: {found:?}"
            );
        }
    }
}

/// The expansions a score blends are the most-held ones, not the first ones in
/// the dictionary (search-engine-developer ref 04 M9): with more words under the
/// prefix than the cap, a lexicographic cut keeps the 64 rare `pfx00`…`pfx63`
/// and drops `pfxcommon`, and the records holding it would score nothing.
#[test]
fn a_blended_prefix_keeps_its_most_held_expansions() {
    let held = store();
    let mut session = Session::new(&held);
    let mut script = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
         DEFINE ANALYZER plain FILTERS lowercase;\n\
         DEFINE COLLECTION notes;\n\
         DEFINE FIELD body ON notes TYPE string ANALYZER plain;\n\
         DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n",
    );
    for rare in 0..70 {
        script.push_str(&format!(
            "CREATE notes:{rare} = {{ body: 'pfx{rare:02}' }};\n"
        ));
    }
    for common in 100..103 {
        script.push_str(&format!(
            "CREATE notes:{common} = {{ body: 'pfxcommon' }};\n"
        ));
    }
    session.run(&script).unwrap();
    let found = ranked(
        &mut session,
        "SELECT id, search::score(body, 'pfx*') AS score FROM notes WHERE body MATCHES 'pfxcommon';",
    );
    assert!(
        found[0].1 > 0.0,
        "the most-held expansion was cut: {found:?}"
    );
}
