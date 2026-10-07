//! G069 C4 — `approx_distinct` and `approx_quantile` (ADR-0122 Part C).
//!
//! Every expected number is worked out from the fixture by hand: a distinct
//! count is the size of a set the test can name, and a quantile is the value at
//! index `floor(q × (n − 1))` of the sorted values, which the answer must be
//! within the declared relative error of.
//!
//! The gathered reads compare the partial node with the leader, which holds
//! every shard. Equality there is bit for bit, because both merges are
//! order-independent (ADR-0122 C5).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Note, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

use crate::gathered_reads::{answer, follower_of_the_middle_of, leader, pair_of, signed_in};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// `obs` holding `n` = 1 to 1000 as integers in group `all`; then, in group
/// `kinds`, `1.0`, decimal `2.00`, the string `'1'`, a null and an absent `n`.
///
/// Distinct present values: the thousand numbers and the string. `1.0` and
/// `2.00` equal `1` and `2` in the value system, so they are not new.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    let mut script = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
         DEFINE DATABASE shop; USE DATABASE shop;\n\
         DEFINE COLLECTION obs;\nBEGIN;\n",
    );
    for n in 1..=1000 {
        script.push_str(&format!("CREATE obs:{n} = {{ n: {n}, g: 'all' }};\n"));
    }
    script.push_str(
        "CREATE obs:1001 = { n: 1.0, g: 'kinds' };\n\
         CREATE obs:1002 = { n: dec 2.00, g: 'kinds' };\n\
         CREATE obs:1003 = { n: '1', g: 'kinds' };\n\
         CREATE obs:1004 = { n: NULL, g: 'kinds' };\n\
         CREATE obs:1005 = { g: 'kinds' };\n\
         COMMIT;",
    );
    session.run(&script).unwrap();
    session
}

/// The fields of the one record a read answers.
fn fields(session: &mut Session<'_>, read: &str) -> BTreeMap<String, Value> {
    let outcomes = session.run(read).unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    assert_eq!(records.len(), 1, "{read}: {records:?}");
    let Value::Object(fields) = &records[0].1 else {
        panic!("{read}: not an object: {:?}", records[0].1);
    };
    fields.clone()
}

fn float(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(Number::Float(held))) => *held,
        other => panic!("not a float: {other:?}"),
    }
}

fn integer(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(Number::Integer(held))) => *held,
        other => panic!("not an integer: {other:?}"),
    }
}

/// `answer` is within relative error `bound` of `expected`; zero only as zero.
fn within(answer: f64, expected: f64, bound: f64) -> bool {
    if expected == 0.0 {
        return answer == 0.0;
    }
    ((answer - expected) / expected).abs() <= bound
}

#[test]
fn a_distinct_count_below_the_small_set_is_exact_and_equal_values_count_once() {
    let store = store();
    let mut session = ready(&store);
    let held = fields(&mut session, "SELECT approx_distinct(n) AS d FROM obs;");
    // 1..=1000 and the string '1'; 1.0, dec 2.00, null and the absent one add nothing.
    assert_eq!(integer(held.get("d")), 1001);
    let kinds = fields(
        &mut session,
        "SELECT approx_distinct(n) AS d FROM obs WHERE g = 'kinds';",
    );
    // 1.0, dec 2.00 and '1': three present values, three distinct.
    assert_eq!(integer(kinds.get("d")), 3);
}

#[test]
fn a_distinct_count_past_the_small_set_is_within_its_bound() {
    let store = store();
    let mut session = ready(&store);
    let mut script =
        String::from("USE NAMESPACE prod; USE DATABASE shop; DEFINE COLLECTION wide; BEGIN;\n");
    for n in 1001..=3000 {
        script.push_str(&format!("CREATE wide:{n} = {{ n: {n} }};\n"));
        script.push_str(&format!("CREATE wide:{} = {{ n: {n} }};\n", n + 10_000));
    }
    script.push_str("COMMIT;");
    session.run(&script).unwrap();
    let held = fields(&mut session, "SELECT approx_distinct(n) AS d FROM wide;");
    let d = integer(held.get("d"));
    // 2 000 distinct values, each written twice; 2.5 % of 2 000 is 50.
    assert!((d - 2000).abs() <= 50, "estimated {d} distinct of 2000");
}

#[test]
fn a_quantile_is_within_its_relative_error_of_the_value_at_its_rank() {
    let store = store();
    let mut session = ready(&store);
    // Sorted 1..=1000; the value at floor(q × 999).
    for (q, expected) in [("0", 1.0), ("0.5", 500.0), ("0.99", 990.0), ("1", 1000.0)] {
        let held = fields(
            &mut session,
            &format!("SELECT approx_quantile(n, {q}) AS v FROM obs WHERE g = 'all';"),
        );
        let v = float(held.get("v"));
        assert!(within(v, expected, 0.01), "q {q}: {v} for {expected}");
    }
}

#[test]
fn a_quantile_places_negatives_below_zero_and_answers_zero_exactly() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION t; CREATE t:1 = { n: -100 }; CREATE t:2 = { n: -10 }; CREATE t:3 = { n: 0 };\n\
             CREATE t:4 = { n: 0.0 }; CREATE t:5 = { n: 10 };",
        )
        .unwrap();
    // Sorted -100, -10, 0, 0, 10; index floor(q × 4).
    for (q, expected) in [("0", -100.0), ("0.25", -10.0), ("0.5", 0.0), ("1", 10.0)] {
        let held = fields(
            &mut session,
            &format!("SELECT approx_quantile(n, {q}) AS v FROM t;"),
        );
        let v = float(held.get("v"));
        assert!(within(v, expected, 0.01), "q {q}: {v} for {expected}");
    }
}

#[test]
fn over_nothing_a_distinct_count_is_zero_and_a_quantile_is_absent() {
    let store = store();
    let mut session = ready(&store);
    // Five records, none holding `missing`.
    let held = fields(
        &mut session,
        "SELECT count(*) AS c, approx_distinct(missing) AS d, approx_quantile(missing, 0.5) AS m \
         FROM obs WHERE g = 'kinds';",
    );
    assert_eq!(integer(held.get("c")), 5);
    assert_eq!(integer(held.get("d")), 0);
    assert_eq!(held.get("m"), None, "{held:?}");
}

#[test]
fn a_quantile_refuses_what_it_cannot_fold_and_a_rank_it_cannot_use() {
    let store = store();
    let mut session = ready(&store);
    for (read, found) in [
        // The string '1' is present and is not a number.
        (
            "SELECT approx_quantile(n, 0.5) AS m FROM obs WHERE g = 'kinds';",
            "string",
        ),
        (
            "SELECT approx_quantile(n, 1.5) AS m FROM obs WHERE g = 'all';",
            "a rank outside 0 to 1",
        ),
        (
            "SELECT approx_quantile(n, 'half') AS m FROM obs WHERE g = 'all';",
            "a rank that is not a number",
        ),
        (
            "SELECT approx_quantile(n, n / 1000) AS m FROM obs WHERE g = 'all';",
            "a rank that differs between records",
        ),
    ] {
        match session.run(read) {
            Err(Error::NotSummable {
                fold, found: was, ..
            }) => {
                assert_eq!(fold, "approx_quantile", "{read}");
                assert_eq!(was, found, "{read}");
            }
            other => panic!("{read}: {other:?}"),
        }
    }
    // The rank is part of the fold, so leaving it out is a mistake in the
    // statement, refused before any record is read.
    match session.run("SELECT approx_quantile(n) AS m FROM obs;") {
        Err(Error::Script(tessari_ql::Error::UnexpectedToken { expected, .. })) => {
            assert_eq!(expected, "`,` and the rank, a number from 0 to 1");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_answer_with_an_approximate_fold_says_so_once_per_fold() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "SELECT g, approx_distinct(n) AS d, approx_distinct(g) AS e, \
             approx_quantile(n, 0.5) AS m, count(*) AS c FROM obs WHERE g = 'all' GROUP BY g;",
        )
        .unwrap();
    let notes = outcomes.last().unwrap().notes();
    let estimated: Vec<&Note> = notes
        .iter()
        .filter(|note| note.kind() == "estimated")
        .collect();
    assert_eq!(
        estimated,
        vec![
            &Note::Estimated {
                fold: "approx_distinct",
                method: "hll-14",
                bound: "0.025",
                collapsed: None,
            },
            &Note::Estimated {
                fold: "approx_quantile",
                method: "ddsketch",
                bound: "0.01",
                collapsed: Some(false),
            },
        ],
        "{notes:?}"
    );
    // An exact read carries no such note.
    let exact = session
        .run("SELECT count(*) AS c, median(n) AS m FROM obs WHERE g = 'all';")
        .unwrap();
    assert!(
        exact
            .last()
            .unwrap()
            .notes()
            .iter()
            .all(|note| note.kind() != "estimated")
    );
}

/// ADR-0122 C5 — on a split table each leader sends its sketch, and the merged
/// answer equals the leader's own walk bit for bit; no record travels.
#[test]
fn a_sketch_is_folded_on_the_leaders_and_equals_a_whole_node() {
    let leader = leader();
    let mut script = String::from(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE TABLE samples (g string, n number) IDENTITY uuid SPLIT AT 'g', 'p';\nBEGIN;\n",
    );
    // 1 500 records over all three shards: 1 200 distinct values in group
    // `wide`, so the merged sketch is past its small set, and 40 in `narrow`.
    for i in 0..1500 {
        let shard = ["a", "h", "q"][i % 3];
        let (g, n) = if i < 1200 {
            ("wide", format!("{}", i * 7))
        } else {
            ("narrow", format!("{}.5", i % 40))
        };
        script.push_str(&format!(
            "CREATE samples:'{shard}{i:05}' = {{ g: '{g}', n: {n} }};\n"
        ));
    }
    script.push_str("COMMIT;");
    signed_in(&leader, "root").run(&script).unwrap();
    let follower = follower_of_the_middle_of(&leader, "samples");
    let pair = pair_of(leader, follower);
    let mut on_the_follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in [
        "SELECT approx_distinct(n) AS d, approx_quantile(n, 0.5) AS m, \
         approx_quantile(n, 0.99) AS p FROM samples;",
        "SELECT g, approx_distinct(n) AS d, approx_quantile(n, 0.25) AS m, count(*) AS c \
         FROM samples GROUP BY g;",
        "SELECT approx_distinct(g) AS d, approx_quantile(n, 1) AS top FROM samples WHERE n > 100;",
    ] {
        let (expected, _) = answer(&mut whole, read);
        assert!(!expected.is_empty(), "{read}: the control answered nothing");
        let (gathered, notes) = answer(&mut on_the_follower, read);
        assert_eq!(gathered, expected, "{read}");
        assert_eq!(pair.sent(), 0, "{read}: records travelled");
        assert!(notes.iter().any(|note| note.kind() == "gathered"), "{read}");
        assert!(
            notes.iter().any(|note| note.kind() == "estimated"),
            "{read}"
        );
    }
}
