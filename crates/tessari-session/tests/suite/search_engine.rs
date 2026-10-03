//! G051 C9 — `DEFINE SEARCH`: several fields and several tables ranked as one
//! collection (ADR-0100 D2, ADR-0105).
//!
//! The ranking is checked against a **brute force**: the corpus is analysed by
//! a lowercase-only chain, so the test can tokenise it itself, recompute BM25F
//! from first principles and compare. Everything else is a differential — the
//! same read under two definitions, two grants or two states of the data — with
//! one side pinned to expected records.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tessari_constants::{BM25_B, BM25_K1};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";
const PASSWORD: &str = "correct horse battery";

/// Notes and articles, analysed by a chain the test can reproduce by hand.
const CORPUS: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
     DEFINE ANALYZER plain FILTERS lowercase;\n\
     DEFINE COLLECTION notes; DEFINE COLLECTION articles;\n\
     CREATE notes:1 = { title: 'Ada Lovelace', body: 'notes on the analytical engine' };\n\
     CREATE notes:2 = { title: 'The engine', body: 'ada wrote the first program for the engine' };\n\
     CREATE notes:3 = { title: 'Babbage', body: 'charles babbage designed it and ada lovelace described it' };\n\
     CREATE notes:4 = { title: 'Cards', body: 'punched cards drove the loom' };\n\
     CREATE articles:1 = { headline: 'Lovelace and the engine', text: 'a note by ada' };\n\
     CREATE articles:2 = { headline: 'Looms', text: 'jacquard cards and patterns' };";

fn store(definition: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session.run(CORPUS).unwrap();
    session.run(definition).unwrap();
    store
}

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(USE).unwrap();
    session
}

/// `(table, id, score)` in the order the read answered.
fn ranked(session: &mut Session<'_>, read: &str) -> Vec<(String, String, f64)> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{read}: {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(id, value)| {
            let Value::Object(fields) = value else {
                panic!("{value:?}");
            };
            let table = match fields.get("source") {
                Some(Value::String(table)) => table.clone(),
                other => panic!("table: {other:?}"),
            };
            let score = match fields.get("score") {
                Some(Value::Number(number)) => number.as_float().unwrap(),
                other => panic!("score: {other:?}"),
            };
            (table, id.to_string(), score)
        })
        .collect()
}

/// The ids a search read answers, in order.
fn ids(session: &mut Session<'_>, search: &str, ask: &str) -> Vec<String> {
    ranked(
        session,
        &format!(
            "SELECT search::table_name() AS source, search::score() AS score \
             FROM SEARCH {search} {ask};"
        ),
    )
    .into_iter()
    .map(|(_, id, _)| id)
    .collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// BM25F over the documents given, each a list of `(weight, text)` fields in
/// the search's field order; the query a list of whole words, all required.
fn brute_force(documents: &[Vec<(f64, &str)>], query: &[&str]) -> Vec<Option<f64>> {
    let fields = documents[0].len();
    let analysed: Vec<Vec<Vec<String>>> = documents
        .iter()
        .map(|document| document.iter().map(|(_, text)| tokens(text)).collect())
        .collect();
    let total = analysed
        .iter()
        .filter(|document| document.iter().any(|field| !field.is_empty()))
        .count();
    let total = real(total);
    let average: Vec<f64> = (0..fields)
        .map(|field| {
            analysed
                .iter()
                .map(|document| real(document[field].len()))
                .sum::<f64>()
                / total
        })
        .collect();
    let holding = |word: &str| {
        real(
            analysed
                .iter()
                .filter(|document| document.iter().flatten().any(|token| token == word))
                .count(),
        )
    };
    analysed
        .iter()
        .zip(documents)
        .map(|(document, declared)| {
            let held = |word: &str| document.iter().flatten().any(|token| token == word);
            if !query.iter().all(|word| held(word)) {
                return None;
            }
            let mut score = 0.0;
            for word in query {
                let mut weighted = 0.0;
                for (field, tokens) in document.iter().enumerate() {
                    let occurrences = real(tokens.iter().filter(|token| *token == word).count());
                    let normalised = 1.0 - BM25_B + BM25_B * (real(tokens.len()) / average[field]);
                    weighted += declared[field].0 * occurrences / normalised;
                }
                let documents_holding = holding(word);
                let idf =
                    (1.0 + (total - documents_holding + 0.5) / (documents_holding + 0.5)).ln();
                score += idf * weighted * (BM25_K1 + 1.0) / (BM25_K1 + weighted);
            }
            Some(score)
        })
        .collect()
}

/// A count as a float: the corpus here is a handful of short texts.
fn real(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap())
}

/// A statement and how its refusal is recognised.
type Refusal = (&'static str, fn(&Error) -> bool);

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * left.abs().max(1.0)
}

const NOTES_SEARCH: &str =
    "DEFINE SEARCH knowledge ON notes FIELDS title WEIGHT 3, body ANALYZER plain;";

const READ: &str = "SELECT search::table_name() AS source, search::score() AS score \
     FROM SEARCH knowledge MATCHES 'ada lovelace';";

#[test]
fn several_fields_rank_as_one_document_by_bm25f() {
    let held = store(NOTES_SEARCH);
    let found = ranked(&mut session(&held), READ);

    let notes: [(&str, &str); 4] = [
        ("Ada Lovelace", "notes on the analytical engine"),
        ("The engine", "ada wrote the first program for the engine"),
        (
            "Babbage",
            "charles babbage designed it and ada lovelace described it",
        ),
        ("Cards", "punched cards drove the loom"),
    ];
    let documents: Vec<Vec<(f64, &str)>> = notes
        .iter()
        .map(|(title, body)| vec![(3.0, *title), (1.0, *body)])
        .collect();
    let mut wanted: Vec<(String, f64)> = brute_force(&documents, &["ada", "lovelace"])
        .iter()
        .enumerate()
        .filter_map(|(at, score)| score.map(|score| ((at + 1).to_string(), score)))
        .collect();
    wanted.sort_by(|left, right| right.1.total_cmp(&left.1));
    // Notes 1 and 3 hold both words; 1 holds them in the title, which weighs 3.
    assert_eq!(
        wanted.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["1", "3"]
    );
    assert_eq!(found.len(), wanted.len(), "{found:?}");
    for ((table, id, score), (want_id, want_score)) in found.iter().zip(&wanted) {
        assert_eq!(table, "notes");
        assert_eq!(id, want_id, "{found:?}");
        assert!(close(*score, *want_score), "{id}: {score} != {want_score}");
    }
}

#[test]
fn one_field_of_weight_one_ranks_as_its_field_index_scores() {
    let held = store(
        "DEFINE SEARCH bodies ON notes FIELDS body ANALYZER plain;\n\
         DEFINE FIELD body ON notes TYPE string ANALYZER plain;\n\
         DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
    );
    let mut reader = session(&held);
    let engine = ranked(
        &mut reader,
        "SELECT search::table_name() AS source, search::score() AS score \
         FROM SEARCH bodies MATCHES 'engine';",
    );
    let outcomes = reader
        .run(
            "SELECT id, search::score(body, 'engine') AS score FROM notes \
             WHERE body MATCHES 'engine' ORDER BY score DESC;",
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(engine.len(), records.len(), "{engine:?} vs {records:?}");
    assert!(!engine.is_empty());
    for ((_, id, score), (field_id, value)) in engine.iter().zip(records) {
        let Value::Object(fields) = value else {
            panic!();
        };
        let Some(Value::Number(field_score)) = fields.get("score") else {
            panic!();
        };
        assert_eq!(id, &field_id.to_string());
        assert!(
            close(*score, field_score.as_float().unwrap()),
            "{score} vs {field_score:?}"
        );
    }
}

#[test]
fn a_weight_moves_a_field_up_the_ranking() {
    // `lovelace` is in note 1's two-word title and note 3's nine-word body, and
    // both fields are equally long against their own averages — so equal
    // weights tie them, and the title's weight alone decides which leads.
    let flat = store("DEFINE SEARCH s ON notes FIELDS title, body ANALYZER plain;");
    let heavy = store("DEFINE SEARCH s ON notes FIELDS title WEIGHT 10, body ANALYZER plain;");
    let light = store("DEFINE SEARCH s ON notes FIELDS title WEIGHT 0.1, body ANALYZER plain;");
    let order = |held: &Store| ids(&mut session(held), "s", "MATCHES 'lovelace'");
    assert_eq!(order(&heavy), ["1", "3"]);
    assert_eq!(order(&light), ["3", "1"]);
    assert_eq!(order(&flat).len(), 2);
}

#[test]
fn tables_rank_together_and_each_answers_under_its_own_grants() {
    let held = store(
        "DEFINE SEARCH knowledge ON notes FIELDS title WEIGHT 3, body \
         ON articles FIELDS headline WEIGHT 3, text ANALYZER plain;\n\
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
    );
    let mut root = Session::new(&held);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "DEFINE USER notes_only ON prod.shop ROLE viewer PASSWORD 'correct horse battery';\n\
         DEFINE USER half ON prod.shop ROLE viewer PASSWORD 'correct horse battery';\n\
         USE NAMESPACE prod; USE DATABASE shop;\n\
         GRANT read ON notes TO notes_only;\n\
         GRANT read ON notes FIELDS body TO half;\n\
         GRANT read ON articles TO half;\n\
         DEFINE COLLECTION ledger;\n\
         DEFINE USER ledger_only ON prod.shop ROLE viewer PASSWORD 'correct horse battery';\n\
         GRANT read ON ledger TO ledger_only;",
    )
    .unwrap();
    // A search is listed to a caller who reads one of its tables, and to nobody else.
    let searches = |name: &str| {
        let mut session = Session::new(&held);
        session.sign_in(name, PASSWORD).unwrap();
        let outcomes = session.run(&format!("{USE} INFO FOR DATABASE;")).unwrap();
        let Some(Outcome::Value(Value::Object(info))) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        info.get("searches").cloned()
    };
    assert_eq!(
        searches("notes_only"),
        Some(Value::Array(vec![Value::from("knowledge")]))
    );
    assert_eq!(searches("ledger_only"), Some(Value::Array(Vec::new())));
    let read = "SELECT search::table_name() AS source, search::score() AS score \
                FROM SEARCH knowledge MATCHES 'lovelace';";
    let everything = ranked(&mut root, read);
    let tables: Vec<&str> = everything
        .iter()
        .map(|(table, _, _)| table.as_str())
        .collect();
    assert!(
        tables.contains(&"notes") && tables.contains(&"articles"),
        "{everything:?}"
    );
    assert!(
        everything.windows(2).all(|pair| pair[0].2 >= pair[1].2),
        "one ranking across tables: {everything:?}"
    );

    let signed = |name: &str| {
        let mut session = Session::new(&held);
        session.sign_in(name, PASSWORD).unwrap();
        session.run(USE).unwrap();
        session
    };
    // A table the reader may not read is not searched at all.
    let notes_only = ranked(&mut signed("notes_only"), read);
    assert!(!notes_only.is_empty());
    assert!(
        notes_only.iter().all(|(table, _, _)| table == "notes"),
        "{notes_only:?}"
    );
    // Its scores are measured against the notes alone, as a search over notes
    // alone would measure them — the articles' statistics do not reach it.
    let alone = ranked(
        &mut session(&store(
            "DEFINE SEARCH knowledge ON notes FIELDS title WEIGHT 3, body ANALYZER plain;",
        )),
        read,
    );
    assert_eq!(notes_only.len(), alone.len());
    for (narrow, whole) in notes_only.iter().zip(&alone) {
        assert_eq!(narrow.1, whole.1);
        assert!(close(narrow.2, whole.2), "{narrow:?} vs {whole:?}");
    }
    // A member whose fields the reader cannot all read is not searched either:
    // its dictionary cannot say which field a word came from.
    let half = ranked(&mut signed("half"), read);
    assert!(!half.is_empty());
    assert!(
        half.iter().all(|(table, _, _)| table == "articles"),
        "{half:?}"
    );
}

#[test]
fn a_conjunction_spans_fields_and_a_phrase_sits_inside_one() {
    let held = store("DEFINE SEARCH s ON notes FIELDS title, body ANALYZER plain;");
    let mut reader = session(&held);
    let mut found = |query: &str| -> BTreeSet<String> {
        ids(&mut reader, "s", &format!("MATCHES '{query}'"))
            .into_iter()
            .collect()
    };
    // `cards` in note 4's title and body, `loom` in its body.
    assert_eq!(found("cards loom"), set(&["4"]));
    assert_eq!(found("\"ada lovelace\""), set(&["1", "3"]));
    // `the engine` is a phrase in note 2's title and body, and not in note
    // 1's body, where `analytical` stands between the two words.
    assert_eq!(found("\"the engine\""), set(&["2"]));
    // `lovelace notes` holds across note 1's two fields, never as a phrase.
    assert_eq!(found("lovelace notes"), set(&["1"]));
    assert!(found("\"lovelace notes\"").is_empty());
    assert_eq!(found("ada NOT babbage"), set(&["1", "2"]));
    assert_eq!(found("babbage OR loom"), set(&["3", "4"]));
}

#[test]
fn a_field_can_refuse_an_operator() {
    let open = store("DEFINE SEARCH s ON notes FIELDS title, body ANALYZER plain;");
    let closed = store(
        "DEFINE SEARCH s ON notes FIELDS title NO FUZZY NO PREFIX NO PHRASE, body ANALYZER plain;",
    );
    let found = |held: &Store, ask: &str| -> BTreeSet<String> {
        ids(&mut session(held), "s", ask).into_iter().collect()
    };
    // `lovelace` is in note 1's title alone and in note 3's body.
    for ask in [
        "MATCHES FUZZY 'lovelacx'",
        "MATCHES PREFIX 'lovel'",
        "MATCHES 'lovel*'",
    ] {
        assert_eq!(found(&open, ask), set(&["1", "3"]), "{ask}");
        assert_eq!(found(&closed, ask), set(&["3"]), "{ask}");
    }
    // `ada lovelace` is a phrase in note 1's title and note 3's body.
    assert_eq!(found(&open, "MATCHES '\"ada lovelace\"'"), set(&["1", "3"]));
    assert_eq!(found(&closed, "MATCHES '\"ada lovelace\"'"), set(&["3"]));
    // A whole word still answers in every field.
    assert_eq!(found(&closed, "MATCHES 'lovelace'"), set(&["1", "3"]));
}

#[test]
fn the_definition_is_said_back_and_survives_a_script_and_a_reopen() {
    let definition = "DEFINE SEARCH knowledge ON articles FIELDS headline WEIGHT 2.5 SNIPPET, \
                      text NO FUZZY ON notes FIELDS title WEIGHT 3, body NO PHRASE ANALYZER plain;";
    let held = store(definition);
    let script = tessari_session::write_script(&held).unwrap().text;
    assert!(script.contains(definition), "{script}");
    assert!(!script.contains("DEFINE INDEX knowledge"), "{script}");

    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&restored).run(&script).unwrap();
    let before = ranked(&mut session(&held), READ);
    assert!(before.len() >= 2, "{before:?}");
    assert_eq!(ranked(&mut session(&restored), READ), before);

    let outcomes = session(&held).run("INFO FOR SEARCH knowledge;").unwrap();
    let Some(Outcome::Value(Value::Object(info))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        info.get("analyzer"),
        Some(&Value::from("plain")),
        "{info:?}"
    );
    assert_eq!(documents(info), [("articles", 2), ("notes", 4)]);
    // A database lists its searches by name, once each whatever their members
    // — the one enumeration that reaches them, since INFO FOR TABLE skips members.
    let outcomes = session(&held).run("INFO FOR DATABASE;").unwrap();
    let Some(Outcome::Value(Value::Object(database))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        database.get("searches"),
        Some(&Value::Array(vec![Value::from("knowledge")])),
        "{database:?}"
    );

    // On disk, closed and opened again.
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let backend = tessari_lsm::LsmBackend::open(
            directory.path(),
            tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
        )
        .unwrap();
        Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
    };
    {
        let disk = open();
        let mut writer = Session::new(&disk);
        writer.run(CORPUS).unwrap();
        writer.run(definition).unwrap();
        assert_eq!(ranked(&mut session(&disk), READ), before);
    }
    let disk = open();
    assert_eq!(ranked(&mut session(&disk), READ), before);
}

/// `(table, documents)` per member, from `INFO FOR SEARCH`.
fn documents(info: &BTreeMap<String, Value>) -> Vec<(&str, i64)> {
    let Some(Value::Array(members)) = info.get("members") else {
        panic!("{info:?}");
    };
    members
        .iter()
        .map(|member| {
            let Value::Object(member) = member else {
                panic!();
            };
            let (Some(Value::String(table)), Some(Value::Number(Number::Integer(count)))) =
                (member.get("table"), member.get("documents"))
            else {
                panic!("{member:?}");
            };
            (table.as_str(), *count)
        })
        .collect()
}

#[test]
fn every_write_keeps_the_search_current() {
    let held = store(NOTES_SEARCH);
    let mut writer = session(&held);
    let loom = |writer: &mut Session<'_>| ids(writer, "knowledge", "MATCHES 'loom'");
    assert_eq!(loom(&mut writer), ["4"]);
    writer
        .run("UPDATE notes:4 MERGE { body: 'punched cards' };")
        .unwrap();
    assert!(
        loom(&mut writer).is_empty(),
        "an update leaves the old words"
    );
    writer
        .run("CREATE notes:5 = { title: 'Loom', body: 'the jacquard loom' };")
        .unwrap();
    assert_eq!(loom(&mut writer), ["5"]);
    writer.run("DELETE notes:5;").unwrap();
    assert!(loom(&mut writer).is_empty());
    // A transaction's own writes are answered inside it.
    let outcomes = writer
        .run(
            "BEGIN; CREATE notes:6 = { title: 'Loom' };\n\
             SELECT search::table_name() AS source, search::score() AS score \
             FROM SEARCH knowledge MATCHES 'loom';\n\
             CANCEL;",
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.get(2) else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        records
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>(),
        ["6"]
    );
    assert!(loom(&mut writer).is_empty());
    // A record with no text in any member field is not a document.
    writer.run("CREATE notes:7 = { other: 'loom' };").unwrap();
    let outcomes = writer.run("INFO FOR SEARCH knowledge;").unwrap();
    let Some(Outcome::Value(Value::Object(info))) = outcomes.last() else {
        panic!();
    };
    assert_eq!(documents(info), [("notes", 4)]);
}

#[test]
fn what_cannot_be_searched_is_refused_by_name() {
    let held = store(NOTES_SEARCH);
    let mut session = session(&held);
    assert!(
        matches!(
            session.run("SELECT * FROM SEARCH nothing MATCHES 'ada';"),
            Err(Error::Unknown {
                entity: "search",
                ..
            })
        ),
        "an undeclared search"
    );
    let refusals: [Refusal; 8] = [
        (
            "DEFINE SEARCH other ON notes FIELDS title ANALYZER nothing;",
            |e| {
                matches!(
                    e,
                    Error::Unknown {
                        entity: "analyzer",
                        ..
                    }
                )
            },
        ),
        (
            "DEFINE SEARCH other ON missing FIELDS title ANALYZER plain;",
            |e| {
                matches!(
                    e,
                    Error::Unknown {
                        entity: "table",
                        ..
                    }
                )
            },
        ),
        (
            "DEFINE SEARCH knowledge ON notes FIELDS title ANALYZER plain;",
            |e| matches!(e, Error::SearchExists { .. }),
        ),
        (
            "DEFINE SEARCH other ON notes FIELDS title, title ANALYZER plain;",
            |e| matches!(e, Error::SearchNamesFieldTwice { .. }),
        ),
        (
            "DEFINE SEARCH other ON notes FIELDS title WEIGHT 0 ANALYZER plain;",
            |e| matches!(e, Error::WeightOutOfRange { .. }),
        ),
        (
            "DEFINE SEARCH other ON notes FIELDS title ON notes FIELDS body ANALYZER plain;",
            |e| matches!(e, Error::SearchNamesTableTwice { .. }),
        ),
        (
            "DEFINE SEARCH other ON notes FIELDS title NO FUZZY NO FUZZY ANALYZER plain;",
            |e| matches!(e, Error::Script(_)),
        ),
        ("DEFINE SEARCH other ON notes FIELDS title;", |e| {
            matches!(e, Error::Script(_))
        }),
    ];
    for (refused, expected) in refusals {
        match session.run(refused) {
            Err(error) => assert!(expected(&error), "{refused}: {error:?}"),
            Ok(_) => panic!("{refused} was accepted"),
        }
    }
    assert!(
        session
            .run("DEFINE SEARCH IF NOT EXISTS knowledge ON notes FIELDS body ANALYZER plain;")
            .is_ok()
    );
    assert!(matches!(
        session.run("DROP ANALYZER plain;"),
        Err(Error::StillDepended {
            depended: tessari_session::Depended::AnalyzerBySearch,
            ..
        })
    ));
    for refused in [
        "SELECT * FROM SEARCH knowledge MATCHES 'ada' SPLIT ON tags;",
        "SELECT * FROM SEARCH knowledge MATCHES 'ada' TIMEOUT 1s;",
    ] {
        assert!(
            matches!(session.run(refused), Err(Error::Script(_))),
            "{refused} was accepted"
        );
    }
    session.run("DROP SEARCH knowledge;").unwrap();
    assert!(matches!(
        session.run(READ),
        Err(Error::Unknown {
            entity: "search",
            ..
        })
    ));
    session.run("DROP ANALYZER plain;").unwrap();
}

/// T9.2 — query-time words: synonyms per field, stop words per search.
#[test]
fn synonyms_answer_per_field_and_stop_words_leave_the_query() {
    let held = store(
        "DEFINE SYNONYMS machines { engine: ['loom', 'machine'] };\n\
         DEFINE STOPWORDS common ['the', 'on', 'for'];\n\
         DEFINE SEARCH s ON notes FIELDS title, body SYNONYMS machines ANALYZER plain STOPWORDS common;",
    );
    let mut reader = session(&held);
    let mut found =
        |ask: &str| -> BTreeSet<String> { ids(&mut reader, "s", ask).into_iter().collect() };
    // `engine` reaches note 4's body through its synonym `loom`; note 4's
    // title holds `Cards`, and the title has no synonyms.
    assert_eq!(found("MATCHES 'engine'"), set(&["1", "2", "4"]));
    // A stop word leaves the conjunction: `the engine` asks for `engine`.
    assert_eq!(found("MATCHES 'the engine'"), found("MATCHES 'engine'"));
    // Every word a stop word asks nothing, and answers nothing.
    assert!(found("MATCHES 'the on'").is_empty());
    // A quoted phrase keeps its stop words, and its words are answered by
    // the field's synonyms in place: note 4's body holds `the loom`.
    assert_eq!(found("MATCHES '\"the engine\"'"), set(&["2", "4"]));

    // Query-time means a set is replaced without touching an index: the
    // search names it, so it is dropped with the search's leave, not under it.
    let mut writer = session(&held);
    assert!(matches!(
        writer.run("DROP SYNONYMS machines;"),
        Err(Error::StillDepended { .. })
    ));
    assert!(matches!(
        writer.run("DROP STOPWORDS common;"),
        Err(Error::StillDepended { .. })
    ));
    for refused in [
        "DEFINE SYNONYMS two { 'a b': ['c'] };",
        "DEFINE SYNONYMS machines { a: ['b'] };",
        "DEFINE SEARCH t ON notes FIELDS title SYNONYMS missing ANALYZER plain;",
        "DEFINE SEARCH t ON notes FIELDS title ANALYZER plain STOPWORDS missing;",
    ] {
        assert!(writer.run(refused).is_err(), "{refused} was accepted");
    }
    // The script carries both sets, and a fresh store answers the same.
    let script = tessari_session::write_script(&held).unwrap().text;
    assert!(
        script.contains("DEFINE SYNONYMS machines { 'engine': ['loom', 'machine'] };"),
        "{script}"
    );
    assert!(
        script.contains("DEFINE STOPWORDS common ['for', 'on', 'the'];"),
        "{script}"
    );
    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&restored).run(&script).unwrap();
    assert_eq!(
        ids(&mut session(&restored), "s", "MATCHES 'the engine'"),
        ids(&mut session(&held), "s", "MATCHES 'the engine'")
    );
}

/// T9.2 — where the query was answered: snippets and marks as byte ranges.
#[test]
fn a_snippet_and_the_marks_are_byte_ranges_of_the_matched_words() {
    let held = store(
        "DEFINE SEARCH s ON notes FIELDS title, body SNIPPET ANALYZER plain;\n\
         CREATE notes:9 = { title: 'Long', body: 'one two three four five six seven eight nine ten \
         eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty \
         twentyone twentytwo twentythree twentyfour babbage and ada lovelace together' };",
    );
    let mut reader = session(&held);
    let outcomes = reader
        .run(
            "SELECT search::snippet() AS snippet, search::highlight(body) AS marks, body \
             FROM SEARCH s MATCHES 'ada lovelace';",
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    let row = |id: &str| {
        records
            .iter()
            .find(|(held, _)| held.to_string() == id)
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    let at = |object: &Value, field: &str| match object {
        Value::Object(fields) => match fields.get(field) {
            Some(Value::Number(Number::Integer(byte))) => usize::try_from(*byte).unwrap(),
            other => panic!("{field}: {other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let Value::Object(nine) = row("9") else {
        panic!();
    };
    let Some(Value::String(body)) = nine.get("body") else {
        panic!();
    };
    // The window holds both words, so it reaches past the twenty-fourth token.
    let snippet = nine.get("snippet").unwrap();
    let text = &body[at(snippet, "start")..at(snippet, "end")];
    assert!(text.contains("ada lovelace"), "{text:?}");
    assert!(!text.starts_with("one "), "{text:?}");
    // Marks are the two words, as bytes of the body.
    let Some(Value::Array(marks)) = nine.get("marks") else {
        panic!();
    };
    let marked: Vec<&str> = marks
        .iter()
        .map(|mark| &body[at(mark, "start")..at(mark, "end")])
        .collect();
    assert_eq!(marked, ["ada", "lovelace"]);
    // Note 1 holds both words in its title, which is no `SNIPPET` field.
    let Value::Object(one) = row("1") else {
        panic!();
    };
    assert_eq!(one.get("snippet"), None, "{one:?}");
    // Outside a search there is no ranked record to answer about.
    assert!(matches!(
        reader.run("SELECT search::snippet() AS s FROM notes;"),
        Err(Error::NotSearched { .. })
    ));
}

/// T9.2 — type-ahead: ranked completions, the floor, and grants.
#[test]
fn completions_are_ranked_by_how_many_records_hold_them() {
    let held = store(
        "DEFINE SEARCH knowledge ON notes FIELDS title, body ON articles FIELDS headline, text \
         ANALYZER plain;\n\
         CREATE notes:5 = { title: 'note', body: 'notes' };\n\
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
    );
    let mut reader = Session::new(&held);
    reader.sign_in("root", PASSWORD).unwrap();
    reader.run(USE).unwrap();
    let completed = |reader: &mut Session<'_>, typed: &str| -> Vec<(String, i64)> {
        let outcomes = reader
            .run(&format!(
                "SELECT term, documents FROM SEARCH knowledge COMPLETE '{typed}';"
            ))
            .unwrap();
        let Some(Outcome::Records { records, .. }) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        records
            .iter()
            .map(|(_, row)| {
                let Value::Object(row) = row else { panic!() };
                let (Some(Value::String(term)), Some(Value::Number(Number::Integer(count)))) =
                    (row.get("term"), row.get("documents"))
                else {
                    panic!("{row:?}");
                };
                (term.clone(), *count)
            })
            .collect()
    };
    // `notes` is held by notes 1 and 5, `note` by note 5 and article 1.
    assert_eq!(
        completed(&mut reader, "not"),
        [("note".to_owned(), 2), ("notes".to_owned(), 2)]
    );
    assert!(matches!(
        reader.run("SELECT term FROM SEARCH knowledge COMPLETE 'no';"),
        Err(Error::PrefixTooShort { .. })
    ));
    // A reader of the notes alone is offered only what the notes hold.
    reader
        .run(
            "DEFINE USER narrow ON prod.shop ROLE viewer PASSWORD 'correct horse battery';\n\
             GRANT read ON notes TO narrow;",
        )
        .unwrap();
    let mut narrow = Session::new(&held);
    narrow.sign_in("narrow", PASSWORD).unwrap();
    narrow.run(USE).unwrap();
    assert_eq!(
        completed(&mut narrow, "not"),
        [("notes".to_owned(), 2), ("note".to_owned(), 1)]
    );
}

/// T9.2 — facets are a grouping over the search, under the reader's grants.
#[test]
fn facets_are_counts_over_the_whole_answer() {
    let held = store(
        "DEFINE SEARCH knowledge ON notes FIELDS title, body ON articles FIELDS headline, text \
         ANALYZER plain;\n\
         UPDATE notes:1 MERGE { kind: 'person' }; UPDATE notes:3 MERGE { kind: 'person' };\n\
         UPDATE notes:2 MERGE { kind: 'machine' }; UPDATE articles:1 MERGE { kind: 'person' };",
    );
    let mut reader = session(&held);
    let outcomes = reader
        .run(
            "SELECT kind, count(*) AS n FROM SEARCH knowledge MATCHES 'ada NOT babbage' \
             GROUP BY kind;",
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    let counts: BTreeMap<String, i64> = records
        .iter()
        .map(|(_, row)| {
            let Value::Object(row) = row else { panic!() };
            let (Some(Value::String(kind)), Some(Value::Number(Number::Integer(n)))) =
                (row.get("kind"), row.get("n"))
            else {
                panic!("{row:?}");
            };
            (kind.clone(), *n)
        })
        .collect();
    // `ada` is in notes 1, 2, 3 and article 1, and note 3 holds `babbage` —
    // a record the postings nominate and the query then refuses.
    assert_eq!(
        counts,
        BTreeMap::from([("machine".to_owned(), 1), ("person".to_owned(), 2)])
    );
    // A ranked read of a search is ordered by its rank and nothing else.
    assert!(matches!(
        reader.run("SELECT * FROM SEARCH knowledge MATCHES 'ada' ORDER BY kind;"),
        Err(Error::SearchIsItsOwnOrder { .. })
    ));
    // EXPLAIN names the search and the postings that serve it.
    let outcomes = reader
        .run("EXPLAIN SELECT * FROM SEARCH knowledge MATCHES 'ada';")
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(plan.get("access"), Some(&Value::from("index")), "{plan:?}");
    assert_eq!(
        plan.get("index"),
        Some(&Value::from("knowledge")),
        "{plan:?}"
    );
}

/// T9.3 — `MATCHES INFIX`, served from a suffix keyspace over the dictionary.
#[test]
fn an_infix_is_answered_by_the_index_as_the_scan_answers_it() {
    let field = "DEFINE FIELD body ON notes TYPE string ANALYZER plain;";
    let indexed = store(&format!(
        "{field}\nDEFINE INDEX by_body ON notes FIELDS body SEARCH;"
    ));
    let scanned = store(field);
    let read = |held: &Store, piece: &str| -> (BTreeSet<String>, Option<Value>) {
        let mut reader = session(held);
        let outcomes = reader
            .run(&format!(
                "SELECT id FROM notes WHERE body MATCHES INFIX '{piece}';\n\
                 EXPLAIN SELECT id FROM notes WHERE body MATCHES INFIX '{piece}';"
            ))
            .unwrap();
        let Some(Outcome::Records { records, .. }) = outcomes.first() else {
            panic!("{outcomes:?}");
        };
        let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        (
            records.iter().map(|(id, _)| id.to_string()).collect(),
            plan.get("shape").cloned(),
        )
    };
    // Pieces inside words, at their ends, at their starts, across two words,
    // and one nothing holds.
    for (piece, expected) in [
        ("ovela", &["3"][..]),
        ("gine", &["1", "2"]),
        ("unch", &["4"]),
        ("ytic", &["1"]),
        ("ada wro", &["2"]),
        ("zzz", &[]),
    ] {
        let (with_index, shape) = read(&indexed, piece);
        let (without, _) = read(&scanned, piece);
        assert_eq!(with_index, without, "{piece}");
        assert_eq!(with_index, set(expected), "{piece}");
        assert_eq!(shape, Some(Value::from("infix-terms")), "{piece}");
    }
    assert!(matches!(
        session(&indexed).run("SELECT id FROM notes WHERE body MATCHES INFIX 'ov';"),
        Err(Error::PrefixTooShort { .. })
    ));
    // The marks are the words holding the piece.
    let outcomes = session(&indexed)
        .run("SELECT search::highlight(body) AS marks, body FROM notes WHERE body MATCHES INFIX 'ovela';")
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!();
    };
    let Value::Object(row) = &records[0].1 else {
        panic!()
    };
    let (Some(Value::Array(marks)), Some(Value::String(body))) =
        (row.get("marks"), row.get("body"))
    else {
        panic!("{row:?}");
    };
    let Value::Object(mark) = &marks[0] else {
        panic!()
    };
    let (Some(Value::Number(Number::Integer(start))), Some(Value::Number(Number::Integer(end)))) =
        (mark.get("start"), mark.get("end"))
    else {
        panic!();
    };
    assert_eq!(
        &body[usize::try_from(*start).unwrap()..usize::try_from(*end).unwrap()],
        "lovelace"
    );
}

/// T9.3 — the suffix keyspace holds exactly the suffixes of the dictionary's
/// terms, after writes that add, keep and remove words.
#[test]
fn the_suffixes_are_exactly_those_of_the_dictionary() {
    use tessari_encoding::{IndexAddress, KeyKind, SearchSuffixKey, SearchTermKey, StoreKey};
    use tessari_kv::{KeyRange, ScanDirection, ScanRequest};

    let backend = Arc::new(MemoryBackend::new());
    let held = Store::open(backend.clone() as Arc<dyn KvBackend>).unwrap();
    let mut writer = Session::new(&held);
    writer.run(CORPUS).unwrap();
    writer
        .run(
            "DEFINE FIELD body ON notes TYPE string ANALYZER plain;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             UPDATE notes:4 MERGE { body: 'punched cards' };\n\
             CREATE notes:7 = { body: 'a jacquard loom' };\n\
             DELETE notes:3;",
        )
        .unwrap();
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
    let index = catalog
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|index| index.name == "by_body")
        .unwrap();
    let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
    let keys = |kind: KeyKind| {
        backend
            .scan(&ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::prefix(&address.prefix(kind)),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>()
    };
    let terms: BTreeSet<String> = keys(KeyKind::SearchTerm)
        .iter()
        .map(|key| {
            SearchTermKey::decode(key.as_slice())
                .unwrap()
                .term
                .as_text()
                .unwrap()
        })
        .collect();
    // The dictionary itself moved: `loom` left with note 4's old body and came
    // back with note 7; `babbage` left with note 3.
    assert!(terms.contains("loom") && terms.contains("jacquard"));
    assert!(!terms.contains("babbage"));
    let mut expected: BTreeSet<(String, String)> = BTreeSet::new();
    expected.insert((String::new(), String::new()));
    for term in &terms {
        let characters: Vec<char> = term.chars().collect();
        for start in 0..characters.len() {
            if characters.len() - start >= 3 {
                expected.insert((characters[start..].iter().collect(), term.clone()));
            }
        }
    }
    let stored: BTreeSet<(String, String)> = keys(KeyKind::SearchSuffix)
        .iter()
        .map(|key| {
            let read = SearchSuffixKey::decode(key.as_slice()).unwrap();
            (read.suffix, read.term)
        })
        .collect();
    assert_eq!(
        stored.difference(&expected).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "orphaned suffixes"
    );
    assert_eq!(
        expected.difference(&stored).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "missing suffixes"
    );
}

/// T9.3 — a search's infix is the same walk over its members' dictionaries.
#[test]
fn a_search_answers_an_infix_from_its_members() {
    let held = store(
        "DEFINE SEARCH s ON notes FIELDS title, body ON articles FIELDS headline, text ANALYZER plain;",
    );
    let mut reader = session(&held);
    let found: BTreeSet<String> = ids(&mut reader, "s", "MATCHES INFIX 'ovela'")
        .into_iter()
        .collect();
    // Note 1's title, note 3's body, article 1's headline — ids, not tables.
    assert_eq!(found, set(&["1", "3"]));
    assert_eq!(ids(&mut reader, "s", "MATCHES INFIX 'ovela'").len(), 3);
    let outcomes = reader
        .run("EXPLAIN SELECT * FROM SEARCH s MATCHES INFIX 'ovela';")
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!();
    };
    assert_eq!(plan.get("access"), Some(&Value::from("index")), "{plan:?}");
}

/// A stemmed search for a misspelling: measured against the words the text
/// held (Q-867), and an exact word outranks a corrected one (the edit-weighted
/// occurrence). `notes:'a'` holds the misspelled `vectro`, `notes:'b'` the word
/// itself; without the weight the two tie and `a` comes first by identity.
#[test]
fn a_fuzzy_search_reaches_surfaces_and_ranks_the_exact_word_first() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:'a' = { body: 'vectro' };\n\
             CREATE notes:'b' = { body: 'vector' };\n\
             CREATE notes:'c' = { body: 'a transaction settles' };\n\
             CREATE notes:'d' = { body: 'transacting traders' };\n\
             DEFINE SEARCH s ON notes FIELDS body ANALYZER english;",
        )
        .unwrap();
    assert_eq!(ids(&mut session, "s", "MATCHES FUZZY 'vector'"), ["b", "a"]);
    assert_eq!(ids(&mut session, "s", "MATCHES FUZZY 'trasnactoin'"), ["c"]);
}

/// **Q-870 — a search the postings can decide is ranked from them, and ranks
/// exactly as the re-analysed text does.**
///
/// Each read runs twice: plain, which the postings answer (plan shape
/// `search from postings`), and with `WHERE true`, which keeps the re-analysis
/// path (plan shape `search`) — the control arm showing the two paths really
/// ran. Records, order and every score bit must agree, across weights, a
/// synonym set on one field only, a prefix, an infix, an `OR` and a `NOT`.
#[test]
fn a_search_ranked_from_postings_equals_the_one_ranked_from_text() {
    let store = store(
        "DEFINE SYNONYMS machines { engine: ['loom', 'machine'] };\n\
         CREATE notes:5 = { title: 'The loom', body: 'weaving' };\n\
         DEFINE SEARCH s ON notes FIELDS title WEIGHT 3, body SYNONYMS machines \
         ON articles FIELDS headline WEIGHT 2, text ANALYZER plain;",
    );
    let mut session = session(&store);
    for ask in [
        "MATCHES 'ada'",
        "MATCHES 'ada lovelace'",
        "MATCHES 'engine'",
        "MATCHES 'ada OR cards'",
        "MATCHES 'ada NOT babbage'",
        "MATCHES PREFIX 'lov'",
        "MATCHES INFIX 'ngin'",
    ] {
        let read = |condition: &str| {
            format!(
                "SELECT search::table_name() AS source, search::score() AS score \
                 FROM SEARCH s {ask}{condition};"
            )
        };
        let shape = |session: &mut Session<'_>, read: &str| {
            let outcomes = session.run(read).unwrap();
            let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
                panic!("{read}: {:?}", outcomes.last());
            };
            plan.shape
        };
        let from_postings = ranked(&mut session, &read(""));
        let from_text = ranked(&mut session, &read(" WHERE true"));
        assert!(!from_postings.is_empty(), "{ask} answered nothing");
        let bits = |answer: &[(String, String, f64)]| {
            answer
                .iter()
                .map(|(table, id, score)| (table.clone(), id.clone(), score.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(&from_postings), bits(&from_text), "{ask}");
        assert_eq!(
            shape(&mut session, &read("")),
            Some("search from postings"),
            "{ask}"
        );
        assert_eq!(
            shape(&mut session, &read(" WHERE true")),
            Some("search"),
            "{ask}"
        );
    }
    // A transaction that wrote a member's table is answered from its text: its
    // own write has no postings yet.
    let outcomes = session
        .run(
            "BEGIN; CREATE notes:9 = { title: 'Ada again', body: 'more ada' }; \
             SELECT search::score() AS score FROM SEARCH s MATCHES 'ada'; COMMIT;",
        )
        .unwrap();
    let answered = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            Outcome::Records { records, plan, .. } => Some((records.len(), plan.shape)),
            _ => None,
        })
        .unwrap();
    assert_eq!(answered, (5, Some("search")));
}
