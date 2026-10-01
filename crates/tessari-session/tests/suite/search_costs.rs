//! G051 T7.6 — what a `SEARCH` index stores is a cost, never an answer
//! (ADR-0100 D4).
//!
//! `POSITIONS`, `OFFSETS` and `NO SCORE` decide what the index keeps beside its
//! postings and so which reads it can serve without re-reading the text. None
//! of them may change what a read answers, and the proof is the store's usual
//! one: every read below is asked of a table carrying the index with those
//! options and of the same rows with no index at all, and the two answers —
//! records and highlight marks alike — are equal.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Outcome, Session};
use tessari_storage::Store;

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// The option sets proven, the plain index first.
const OPTIONS: [&str; 5] = [
    "",
    " POSITIONS",
    " OFFSETS",
    " NO SCORE",
    " POSITIONS OFFSETS NO SCORE",
];

/// Rows with phrases in both orders, stemmed forms, a repeated pair and words
/// sharing beginnings, under the `english` chain.
fn noted(index: Option<&str>) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = { body: 'Ada Lovelace wrote the first program' };\n\
             CREATE notes:2 = { body: 'Lovelace, Ada: the reversed name' };\n\
             CREATE notes:3 = { body: 'Running vectors over a vectorised loop' };\n\
             CREATE notes:4 = { body: 'Lock contention, and contention again, under the lock' };\n\
             CREATE notes:5 = { body: 'Ada wrote programs; Lovelace wrote notes' };\n\
             CREATE notes:6 = { body: 'Café owners write about vector search' };",
        )
        .unwrap();
    if let Some(options) = index {
        session
            .run(&format!(
                "DEFINE INDEX by_body ON notes FIELDS body SEARCH{options};"
            ))
            .unwrap();
    }
    store
}

/// Every form of the language's text search, and the marks it leaves.
const READS: [&str; 13] = [
    "SELECT id FROM notes WHERE body MATCHES 'ada';",
    "SELECT id FROM notes WHERE body MATCHES 'ada lovelace';",
    "SELECT id FROM notes WHERE body MATCHES 'ada OR vector';",
    "SELECT id FROM notes WHERE body MATCHES 'ada NOT program';",
    "SELECT id FROM notes WHERE body MATCHES '\"ada lovelace\"';",
    "SELECT id FROM notes WHERE body MATCHES '\"ada wrote\"~2';",
    "SELECT id FROM notes WHERE body MATCHES '\"ada lov\"*';",
    "SELECT id FROM notes WHERE body MATCHES 'vecto*';",
    "SELECT id FROM notes WHERE body MATCHES PREFIX 'lov wro';",
    "SELECT id FROM notes WHERE body MATCHES FUZZY 'contetnion';",
    "SELECT id, search::highlight(body) AS marks FROM notes WHERE body MATCHES '\"lock contention\"';",
    "SELECT id, search::highlight(body) AS marks FROM notes WHERE body MATCHES 'ada OR cafe';",
    "SELECT id, search::highlight(body) AS marks FROM notes WHERE body MATCHES PREFIX 'vecto';",
];

/// What a read answered, as text, and the path it took.
fn answer(session: &mut Session<'_>, read: &str) -> (String, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("{read}: {:?}", outcomes.last());
    };
    (format!("{records:?}"), plan.access)
}

#[test]
fn every_cost_option_answers_what_the_scan_answers() {
    let scanned = noted(None);
    let mut without = Session::new(&scanned);
    without.run(USE).unwrap();
    for options in OPTIONS {
        let indexed = noted(Some(options));
        let mut with = Session::new(&indexed);
        with.run(USE).unwrap();
        for read in READS {
            let (expected, _) = answer(&mut without, read);
            let (found, path) = answer(&mut with, read);
            assert_eq!(found, expected, "SEARCH{options}: {read} ({path:?})");
        }
        // And the index is used, so the equality is between two paths.
        let (_, path) = answer(&mut with, READS[1]);
        assert_ne!(path, AccessPath::Scan, "SEARCH{options} served nothing");
    }
}

#[test]
fn an_unscored_index_refuses_a_score_as_no_index_does() {
    for (options, refused) in [
        ("", false),
        (" NO SCORE", true),
        (" POSITIONS NO SCORE", true),
    ] {
        let held = noted(Some(options));
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        for call in ["search::score", "search::explain"] {
            let ran = session.run(&format!(
                "SELECT {call}(body, 'ada') AS s FROM notes WHERE body MATCHES 'ada';"
            ));
            if refused {
                assert!(
                    matches!(ran, Err(Error::NoSearchIndex { ref field, .. }) if field == "body"),
                    "SEARCH{options}: {call}: {ran:?}"
                );
            } else {
                assert!(ran.is_ok(), "SEARCH{options}: {call}: {ran:?}");
            }
        }
    }
}

#[test]
fn the_options_are_said_back_and_survive_a_script() {
    let held = noted(Some(" OFFSETS NO SCORE POSITIONS"));
    let script = tessari_session::write_script(&held).unwrap().text;
    let declared = "DEFINE INDEX by_body ON notes FIELDS body SEARCH POSITIONS OFFSETS NO SCORE;";
    assert!(script.contains(declared), "{script}");
    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&restored);
    session.run(&script).unwrap();
    assert!(
        tessari_session::write_script(&restored)
            .unwrap()
            .text
            .contains(declared)
    );
    // A plain index is still written as it always was.
    let plain = tessari_session::write_script(&noted(Some("")))
        .unwrap()
        .text;
    assert!(
        plain.contains("DEFINE INDEX by_body ON notes FIELDS body SEARCH;"),
        "{plain}"
    );
}

#[test]
fn a_cost_option_belongs_to_a_search_index_and_is_said_once() {
    let held = noted(None);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();
    for refused in [
        "DEFINE INDEX i ON notes FIELDS body POSITIONS;",
        "DEFINE INDEX i ON notes FIELDS body UNIQUE OFFSETS;",
        "DEFINE INDEX i ON notes FIELDS body SEARCH POSITIONS POSITIONS;",
        "DEFINE INDEX i ON notes FIELDS body SEARCH NO SCORE NO SCORE;",
        "DEFINE INDEX i ON notes FIELDS body SEARCH NO POSITIONS;",
    ] {
        assert!(
            matches!(session.run(refused), Err(Error::Script(_))),
            "{refused} was accepted"
        );
    }
}

#[test]
fn a_phrase_on_an_index_keeping_positions_is_decided_there() {
    let read = "SELECT id FROM notes WHERE body MATCHES '\"ada lovelace\"';";
    for (options, shape) in [
        ("", "terms"),
        (" OFFSETS", "terms"),
        (" POSITIONS", "phrase"),
        (" POSITIONS NO SCORE", "phrase"),
    ] {
        let held = noted(Some(options));
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        let outcomes = session.run(read).unwrap();
        let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
            panic!("{read}");
        };
        assert_eq!(plan.shape, Some(shape), "SEARCH{options}: {plan:?}");
    }
}

/// What an option promises is stored: `ada` is the first token of `notes:1`,
/// bytes 0 to 3, and an index without the option stores neither list.
#[test]
fn the_index_stores_the_lists_it_promises() {
    for (options, positions, offsets) in [
        ("", Vec::new(), Vec::new()),
        (" POSITIONS", vec![0_u32], Vec::new()),
        (" OFFSETS", Vec::new(), vec![(0_u32, 3_u32)]),
        (" POSITIONS OFFSETS NO SCORE", vec![0], vec![(0, 3)]),
    ] {
        let held = noted(Some(options));
        let transaction = held.begin().unwrap();
        let mut reading = held.begin().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut reading);
        let table = catalog
            .table_id(
                tessari_types::NamespaceId::new(1),
                tessari_types::DatabaseId::new(1),
                "notes",
            )
            .unwrap()
            .unwrap();
        let index = catalog.indexes_on(table).unwrap().remove(0);
        let found = transaction
            .located(&index, "ada", &tessari_types::RecordId::Int(1))
            .unwrap()
            .unwrap();
        assert_eq!(found.positions, positions, "SEARCH{options}");
        assert_eq!(found.offsets, offsets, "SEARCH{options}");
    }

    // `NO SCORE` keeps membership alone and no collection statistics — on the
    // build of the index and on a write made after it.
    for (options, documents, counted) in [("", 7, true), (" NO SCORE", 0, false)] {
        let held = noted(Some(options));
        let mut writer = Session::new(&held);
        writer.run(USE).unwrap();
        writer
            .run("CREATE notes:7 = { body: 'written after the index' };")
            .unwrap();
        let transaction = held.begin().unwrap();
        let mut reading = held.begin().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut reading);
        let table = catalog
            .table_id(
                tessari_types::NamespaceId::new(1),
                tessari_types::DatabaseId::new(1),
                "notes",
            )
            .unwrap()
            .unwrap();
        let index = catalog.indexes_on(table).unwrap().remove(0);
        assert_eq!(
            transaction.search_statistics(&index).unwrap().documents,
            documents,
            "SEARCH{options}"
        );
        let posting = transaction
            .posting(&index, "ada", &tessari_types::RecordId::Int(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            matches!(posting, tessari_encoding::Posting::Counted { .. }),
            counted,
            "SEARCH{options}: {posting:?}"
        );
    }
}
