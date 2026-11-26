//! `LET` and `RETURN` — the two statements that let one engine's answer reach
//! the next statement.
//!
//! Every read in this language resolves to exactly one access path, chosen by
//! the shape of the statement. That is what keeps a read's cost legible, and it
//! is also why a question crossing two engines could not be *said*: the nearest
//! neighbours by embedding, and then who follows them, is a vector read and a
//! graph walk, and nothing carried the first answer into the second.
//!
//! A binding carries it. What these tests hold to is one sentence: **a bound
//! value behaves exactly as a caller-supplied one does** — it is substituted
//! into the statements below it before they run, so nothing downstream can tell
//! which of the two happened, and the planner still meets a literal where it
//! looks for one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// A tenancy, three people, and the edges between them.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada', city: 'london', age: 36 };\n\
             CREATE users:2 = { name: 'grace', city: 'london', age: 45 };\n\
             CREATE users:3 = { name: 'alan', city: 'cambridge', age: 41 };",
        )
        .unwrap();
    session
}

fn rows(outcome: &Outcome) -> usize {
    outcome.records().map_or(0, <[_]>::len)
}

#[test]
fn a_binding_reaches_the_statement_below_it() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now("LET $city = 'london'; SELECT * FROM users WHERE city = $city;")
        .unwrap();
    // The binding itself reports `Done`: its value is not the script's answer,
    // it is what the next statement was written against.
    assert_eq!(outcomes[0], Outcome::Done);
    assert_eq!(rows(&outcomes[1]), 2);
}

#[test]
fn a_bound_value_still_reaches_an_index() {
    // The property the whole design rests on. Substitution happens before the
    // statement runs, so the planner meets a literal on the right-hand side and
    // chooses the index — a binding resolved at evaluation instead would have
    // silently dropped it, and the read would still have answered correctly
    // while costing a scan.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now(
            "DEFINE INDEX by_city ON users FIELDS city;\n\
             LET $city = 'london';\n\
             SELECT * FROM users WHERE city = $city;",
        )
        .unwrap();
    assert_eq!(outcomes[2].path(), Some(AccessPath::Index));
}

#[test]
fn one_engines_answer_becomes_the_next_statements_question() {
    // The reason this feature exists. The first read resolves to one access
    // path and the second to another; what travels between them is a value.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now(
            "LET $names = SELECT name FROM users WHERE city = 'london';\n\
             RETURN $names;",
        )
        .unwrap();
    let Some(Value::Array(answered)) = outcomes[1].value() else {
        panic!("expected the bound array back, got {:?}", outcomes[1]);
    };
    assert_eq!(answered.len(), 2);
}

#[test]
fn a_binding_composes_with_a_fold() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now("LET $floor = 40; SELECT count(*) AS n FROM users WHERE age > $floor;")
        .unwrap();
    let records = outcomes[1].records().unwrap();
    assert_eq!(records.len(), 1);
}

#[test]
fn a_return_names_the_scripts_answer() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now("LET $a = 6; LET $b = 7; RETURN $a * $b;")
        .unwrap();
    assert_eq!(
        outcomes[2].value(),
        Some(&Value::Number(Number::Integer(42)))
    );
}

#[test]
fn a_binding_stands_where_a_record_identity_does() {
    // The same rule a caller's parameter follows: a value may stand for the
    // **id** half of an identity and never for the table half.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now("LET $who = 2; SELECT * FROM users:$who;")
        .unwrap();
    assert_eq!(rows(&outcomes[1]), 1);
}

#[test]
fn a_name_is_bound_once_in_a_script() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run_now("LET $x = 1; LET $x = 2; RETURN $x;");
    assert!(
        format!("{}", failed.unwrap_err()).contains("bound twice"),
        "a rebinding should be refused where it is written"
    );
}

#[test]
fn a_script_answers_with_one_value() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run_now("RETURN 1; RETURN 2;");
    assert!(
        format!("{}", failed.unwrap_err()).contains("one value"),
        "two answers should be refused where they are written"
    );
}

#[test]
fn a_name_written_above_its_binding_is_refused_before_anything_runs() {
    // The property `Script::bind` already had, kept: a script either binds or
    // does nothing. `$x` here is bound by no caller and by no *preceding*
    // `LET`, so it fails before the write below it reaches the store.
    let store = store();
    let mut session = ready(&store);
    let failed = session.run_now(
        "CREATE users:9 = { name: $x };\n\
         LET $x = 'too late';",
    );
    assert!(failed.is_err(), "a forward reference should be refused");
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let outcomes = session.run_now("SELECT * FROM users:9;").unwrap();
    assert_eq!(rows(&outcomes[0]), 0, "nothing should have been written");
}

#[test]
fn a_caller_and_a_script_may_not_name_the_same_thing() {
    // Refused rather than resolved by precedence: either reading silently
    // discards a value somebody supplied on purpose.
    let store = store();
    let mut session = ready(&store);
    let mut supplied = Parameters::new();
    supplied.insert("city".to_owned(), Value::String("london".to_owned()));
    let failed = session.run_with_now(
        "LET $city = 'cambridge'; SELECT * FROM users WHERE city = $city;",
        &supplied,
    );
    assert!(
        format!("{}", failed.unwrap_err()).contains("supplied by the caller"),
        "a collision should name both halves"
    );
}

#[test]
fn a_binding_and_a_caller_value_compose() {
    let store = store();
    let mut session = ready(&store);
    let mut supplied = Parameters::new();
    supplied.insert("floor".to_owned(), Value::Number(Number::Integer(40)));
    let outcomes = session
        .run_with_now(
            "LET $city = 'london'; SELECT * FROM users WHERE city = $city AND age > $floor;",
            &supplied,
        )
        .unwrap();
    assert_eq!(rows(&outcomes[1]), 1);
}

#[test]
fn a_binding_survives_a_transaction() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run_now(
            "BEGIN;\n\
             LET $city = 'cambridge';\n\
             CREATE users:4 = { name: 'joan', city: $city, age: 29 };\n\
             COMMIT;\n\
             SELECT * FROM users WHERE city = 'cambridge';",
        )
        .unwrap();
    assert_eq!(rows(&outcomes[4]), 2);
}

// ---- G099 W3 (C3): the statement compile cache ----

/// A repeated statement answers and plans identically — the compiled form a
/// cache hands back is the same tree parse+bind produced.
#[test]
fn a_repeated_statement_answers_and_plans_identically() {
    let store = store();
    let mut session = ready(&store);
    let script = "SELECT * FROM users WHERE city = 'london';";
    let first = session.run_now(script).unwrap();
    let again = session.run_now(script).unwrap();
    assert_eq!(rows(first.last().unwrap()), 2);
    assert_eq!(
        first.last().unwrap().plan().unwrap().access,
        again.last().unwrap().plan().unwrap().access,
        "the cached compile plans the way the fresh one did"
    );
    assert_eq!(rows(again.last().unwrap()), 2);
    assert!(
        session.compile_cache_len() > 0,
        "a repeated statement lands in the compile cache"
    );
}

/// Bound values partition the cache: the same text under different
/// parameters is a different compiled statement, and each answers its own.
#[test]
fn bound_values_partition_the_compile_cache() {
    let store = store();
    let mut session = ready(&store);
    // The fixture's own script is a cached entry; the assertion counts the
    // delta the three bound forms add.
    let before = session.compile_cache_len();
    for (name, expect) in [("ada", 1_usize), ("grace", 1), ("nobody", 0)] {
        let mut parameters = Parameters::new();
        parameters.insert("who".to_owned(), Value::String(name.to_owned()));
        let outcomes = session
            .run_with_now("SELECT * FROM users WHERE name = $who;", &parameters)
            .unwrap();
        assert_eq!(rows(outcomes.last().unwrap()), expect, "{name}");
    }
    assert_eq!(
        session.compile_cache_len(),
        before + 3,
        "three distinct bound forms, three entries"
    );
}

/// The cache never freezes catalog-derived behaviour: a catalog change
/// between two identical calls is visible in the second call's plan — names
/// re-resolve at run, and the catalog rows themselves invalidate the way
/// they always have (G072).
#[test]
fn a_catalog_change_between_identical_calls_is_visible() {
    let store = store();
    let mut session = ready(&store);
    let script = "SELECT * FROM users WHERE city = 'london';";
    let before = session.run_now(script).unwrap();
    assert_eq!(
        before.last().unwrap().plan().unwrap().access,
        AccessPath::Scan,
        "no index yet: the walk answers"
    );
    session
        .run_now("DEFINE INDEX by_city ON users FIELDS city;")
        .unwrap();
    let after = session.run_now(script).unwrap();
    assert_eq!(rows(after.last().unwrap()), 2, "the answer does not change");
    assert_eq!(
        after.last().unwrap().plan().unwrap().access,
        AccessPath::Index,
        "the cached compile re-resolved: the new index serves the second call"
    );
}

/// The cache is bounded: past the capacity the oldest entries give way, and
/// the cache never grows past the constant.
#[test]
fn the_compile_cache_is_bounded() {
    let store = store();
    let mut session = ready(&store);
    let cap = tessari_constants::COMPILE_CACHE_MAX;
    for at in 0..cap.saturating_add(4) {
        session
            .run_now(&format!("SELECT * FROM users WHERE name = 'n{at}';"))
            .unwrap();
    }
    assert_eq!(
        session.compile_cache_len(),
        cap,
        "the cache holds at most the capacity"
    );
    // The oldest statement was evicted: it compiles afresh — which answers
    // identically, the property the cache may never break.
    let outcomes = session
        .run_now("SELECT * FROM users WHERE name = 'n0';")
        .unwrap();
    assert_eq!(rows(outcomes.last().unwrap()), 0);
}

/// C3's measured figure: the compile a cache hit saves. The miss path runs
/// distinct statements (every one parses+binds); the hit path runs one
/// statement repeatedly (every run after the first is a cache hit). Both
/// include the same execution, so the difference is the compile. Timings
/// print rather than assert, the C3 idiom — what is asserted is exactness.
#[test]
fn c3_compile_latency_before_and_after() {
    let store = store();
    let mut session = ready(&store);
    const K: usize = 200;

    // Hit path: one statement, K runs, the first compile discounted by
    // running it once before the clock starts.
    let repeated = "SELECT * FROM users WHERE city = 'london' AND age > 30;";
    session.run_now(repeated).unwrap();
    let started = std::time::Instant::now();
    for _ in 0..K {
        let outcomes = session.run_now(repeated).unwrap();
        assert_eq!(rows(outcomes.last().unwrap()), 2);
    }
    let hit_ns = started.elapsed().as_nanos() / (K as u128);

    // Miss path: K distinct statements, every one a fresh compile. K stays
    // under the cache's capacity so eviction does not charge the miss path.
    let started = std::time::Instant::now();
    for at in 0..K {
        let outcomes = session
            .run_now(&format!("SELECT * FROM users WHERE name = 'n{at}';"))
            .unwrap();
        assert_eq!(rows(outcomes.last().unwrap()), 0);
    }
    let miss_ns = started.elapsed().as_nanos() / (K as u128);

    // The figures print; a timing ordering is not asserted (a timing
    // assertion is a flake wearing a criterion — the C3 idiom). What the
    // suite holds is exactness: every run above answered the rows its
    // statement names.
    eprintln!(
        "C3 compile figures ({K} runs each): miss path {miss_ns} ns/run, hit path {hit_ns} ns/run, parse+bind saved ≈ {} ns/run",
        miss_ns.saturating_sub(hit_ns),
    );
}
