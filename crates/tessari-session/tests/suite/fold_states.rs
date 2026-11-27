//! `fold_state` — a fold's state as a first-class value (G102 S11): a group
//! can answer its STATE instead of its finish, the state can be stored in a
//! field, merged with states of the same fold, and finished — and the answer
//! equals the direct fold over every record, which is the oracle.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use tessari_session::{RunNow, Session};
use tessari_types::Value;

use super::materialized_views::{ready, store};

fn rows(session: &mut Session<'_>, script: &str) -> Vec<Value> {
    let outcomes = session.run_now(script).unwrap();
    outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(_, value)| value.clone())
        .collect()
}

/// `{band → f}` out of a grouped two-field answer.
fn banded(rows: &[Value]) -> BTreeMap<String, Value> {
    rows.iter()
        .map(|row| {
            let Value::Object(fields) = row else {
                panic!("not an object: {row:?}");
            };
            let Some(Value::Number(band)) = fields.get("band") else {
                panic!("no band in {fields:?}");
            };
            (
                band.to_string(),
                fields.get("f").cloned().unwrap_or(Value::None),
            )
        })
        .collect()
}

fn refusal(session: &mut Session<'_>, script: &str) -> String {
    match session.run_now(script) {
        Err(refused) => refused.to_string(),
        Ok(outcomes) => panic!("`{script}` answered {outcomes:?}"),
    }
}

#[test]
fn a_state_finished_equals_the_direct_fold() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE halves SCHEMAFULL;\n\
             DEFINE FIELD band ON halves TYPE int;\n\
             DEFINE FIELD part ON halves TYPE fold_state;",
        )
        .unwrap();
    session
        .run_now(
            "INSERT INTO halves (band, part) SELECT band, state::of(sum(total)) AS part \
             FROM orders GROUP BY band;",
        )
        .unwrap();
    let finished = banded(&rows(
        &mut session,
        "SELECT band, state::finish(state::merge(part)) AS f FROM halves GROUP BY band;",
    ));
    let direct = banded(&rows(
        &mut session,
        "SELECT band, sum(total) AS f FROM orders GROUP BY band;",
    ));
    assert_eq!(finished.len(), 4);
    assert_eq!(finished, direct, "state→finish diverged from the fold");
}

#[test]
fn states_merge_across_halves_like_one_walk() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE halves SCHEMAFULL;\n\
             DEFINE FIELD band ON halves TYPE int;\n\
             DEFINE FIELD part ON halves TYPE fold_state;",
        )
        .unwrap();
    // Two halves of the table stated separately — the ADR-0114 part-merge
    // property, asked at the statement level.
    for bound in ["n < 20", "n >= 20"] {
        session
            .run_now(&format!(
                "INSERT INTO halves (band, part) SELECT band, state::of(sum(total)) AS part \
                 FROM orders WHERE {bound} GROUP BY band;"
            ))
            .unwrap();
    }
    let merged = banded(&rows(
        &mut session,
        "SELECT band, state::finish(state::merge(part)) AS f FROM halves GROUP BY band;",
    ));
    let direct = banded(&rows(
        &mut session,
        "SELECT band, sum(total) AS f FROM orders GROUP BY band;",
    ));
    assert_eq!(merged, direct, "the merged halves diverged from the whole");
}

#[test]
fn every_distributive_and_part_mergeable_fold_round_trips() {
    let store = store();
    let mut session = ready(&store);
    // The folds whose states travel (ADR-0114's set): each stated, stored,
    // finished — and equal to the direct fold.
    for (fold, spelled) in [
        ("count(*)", "count(*)"),
        ("sum(total)", "sum(total)"),
        ("mean(total)", "mean(total)"),
        ("min(total)", "min(total)"),
        ("max(total)", "max(total)"),
        ("variance(total)", "variance(total)"),
        ("stddev(total)", "stddev(total)"),
    ] {
        session
            .run_now("DEFINE TABLE one_state SCHEMAFULL; DEFINE FIELD part ON one_state TYPE fold_state;")
            .unwrap();
        session
            .run_now(&format!(
                "INSERT INTO one_state (part) SELECT state::of({fold}) AS part FROM orders GROUP BY band LIMIT 4;",
            ))
            .unwrap();
        let finished = rows(
            &mut session,
            "SELECT state::finish(part) AS f FROM one_state;",
        );
        let direct = rows(
            &mut session,
            &format!("SELECT {spelled} AS f FROM orders GROUP BY band LIMIT 4;"),
        );
        let f = |rows: &[Value]| {
            let mut held: Vec<Value> = rows
                .iter()
                .map(|row| match row {
                    Value::Object(fields) => fields.get("f").cloned().unwrap_or(Value::None),
                    _ => panic!("{row:?}"),
                })
                .collect();
            held.sort_by(|one, two| one.partial_cmp(two).unwrap());
            held
        };
        assert_eq!(f(&finished), f(&direct), "the {fold} state diverged");
        session.run_now("DROP TABLE one_state;").unwrap();
    }
}

#[test]
fn a_state_of_another_fold_is_refused_by_name() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE mixed SCHEMAFULL;\n\
             DEFINE FIELD part ON mixed TYPE fold_state;",
        )
        .unwrap();
    session
        .run_now(
            "INSERT INTO mixed (part) SELECT state::of(sum(total)) AS part FROM orders \
             WHERE band = 0 GROUP BY band;",
        )
        .unwrap();
    session
        .run_now(
            "INSERT INTO mixed (part) SELECT state::of(count(*)) AS part FROM orders \
             WHERE band = 1 GROUP BY band;",
        )
        .unwrap();
    // A count accumulator meeting a sum's state refuses rather than mis-adds.
    let refused = refusal(
        &mut session,
        "SELECT state::finish(state::merge(part)) AS f FROM mixed GROUP BY true;",
    );
    assert!(
        refused.contains("cannot merge into"),
        "the refusal did not name the mismatch: {refused}"
    );
}

#[test]
fn finishing_what_is_not_a_state_is_refused() {
    let store = store();
    let mut session = ready(&store);
    let refused = refusal(
        &mut session,
        "SELECT state::finish(3) AS f FROM orders LIMIT 1;",
    );
    assert!(
        refused.contains("answers a fold state"),
        "the refusal did not name the kind: {refused}"
    );
}

#[test]
fn a_field_of_states_admits_nothing_else() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE kept_states SCHEMAFULL;\n\
             DEFINE FIELD part ON kept_states TYPE fold_state;",
        )
        .unwrap();
    let refused = refusal(&mut session, "CREATE kept_states = { part: 'text' };");
    assert!(
        refused.contains("fold_state") || refused.contains("FieldTypeMismatch"),
        "the wrong kind was admitted: {refused}"
    );
}

#[test]
fn the_paired_folds_answer_and_their_states_travel() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE pairs SCHEMAFULL;\n\
             DEFINE FIELD band ON pairs TYPE int;\n\
             DEFINE FIELD x ON pairs TYPE float;\n\
             DEFINE FIELD y ON pairs TYPE float;",
        )
        .unwrap();
    let mut script = String::new();
    for (n, (x, y)) in [(1.0, 2.0), (2.0, 1.0), (3.0, 4.0), (4.0, 3.0), (5.0, 5.0)]
        .iter()
        .enumerate()
    {
        script.push_str(&format!(
            "CREATE pairs:{n} = {{ band: {}, x: {x:.1}, y: {y:.1} }};\n",
            n % 2
        ));
    }
    session.run_now(&script).unwrap();
    let answered = |session: &mut Session<'_>, script: &str| -> f64 {
        let held = rows(session, script);
        let Some(Value::Object(fields)) = held.first() else {
            panic!("no row for `{script}`");
        };
        fields
            .get("f")
            .unwrap_or_else(|| panic!("no f in {fields:?}"))
            .to_string()
            .parse::<f64>()
            .unwrap()
    };
    // x = [1..5] against y = [2,1,4,3,5]: the accumulator suite's
    // hand-checked set, asked end to end.
    for (fold, wanted) in [
        ("corr(x, y)", 0.8),
        ("covar(x, y)", 2.0),
        ("regr_slope(x, y)", 0.8),
        ("regr_intercept(x, y)", 0.6),
        ("regr_r2(x, y)", 0.64),
    ] {
        let got = answered(&mut session, &format!("SELECT {fold} AS f FROM pairs;"));
        assert!((got - wanted).abs() < 1e-9, "{fold} answered {got}");
    }
    // Two halves stated separately, merged and finished — the ADR-0114
    // part-merge property, asked of a paired fold.
    session
        .run_now(
            "DEFINE TABLE pair_halves SCHEMAFULL;\n\
             DEFINE FIELD band ON pair_halves TYPE int;\n\
             DEFINE FIELD part ON pair_halves TYPE fold_state;",
        )
        .unwrap();
    for bound in ["band = 0", "band = 1"] {
        session
            .run_now(&format!(
                "INSERT INTO pair_halves (band, part) SELECT band, state::of(corr(x, y)) AS part \
                 FROM pairs WHERE {bound} GROUP BY band;"
            ))
            .unwrap();
    }
    let merged = answered(
        &mut session,
        "SELECT state::finish(state::merge(part)) AS f FROM pair_halves;",
    );
    assert!(
        (merged - 0.8).abs() < 1e-9,
        "the merged paired state answered {merged}"
    );
}
