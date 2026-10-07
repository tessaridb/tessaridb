//! G068 C2 — `median`, `collect` and the counter folds over a split table are
//! folded on the shards' leaders (ADR-0121): no record of a shard this node
//! lacks travels, and the answer is the one a node holding every shard gives.
//!
//! Every read is compared with the same statement on the leader, which holds
//! every shard. The counter fixtures are also counted by hand, because two
//! answers agreeing says nothing when both came out of one shared mistake.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_constants::GATHER_RECORDS;
use tessari_session::{Asked, Gather, Gathered, Partial, Reduced, Unanswered};
use tessari_types::{Number, RecordId, Value};

use crate::gathered_reads::{
    Pair, answer, follower_of_the_middle_of, leader, pair, pair_of, refused, signed_in,
};

/// `readings`, split at 'g' and 'p' as `ledger` is, holding:
///
/// - `m1`, a counter in time order across the three shards, reset across the
///   boundary of the second and the third (9 at 40 s, then 2 at 50 s);
/// - `m2`, whose samples interleave in time across the shards;
/// - `m3`, float samples whose rises are 0.1, 0.2 and 0.3;
/// - `m4`, three records holding no `n` at all, one per shard.
///
/// The follower holds the middle shard; `narrow` may read `meter` and `at`.
fn readings() -> Pair {
    let leader = leader();
    let mut script = String::from(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE TABLE readings (meter string, at datetime, n number) IDENTITY uuid \
         SPLIT AT 'g', 'p';\n",
    );
    for (id, meter, second, n) in [
        ("a", "m1", 10, Some("1")),
        ("b", "m2", 10, Some("1")),
        ("c", "m1", 20, Some("4")),
        ("d", "m2", 30, Some("5")),
        ("e", "m3", 1, Some("0.0")),
        ("ea", "m4", 5, None),
        ("f", "m3", 2, Some("0.1")),
        ("h", "m1", 30, Some("6")),
        ("ha", "m4", 6, None),
        ("i", "m3", 3, Some("0.0")),
        ("j", "m3", 4, Some("0.2")),
        ("k", "m1", 40, Some("9")),
        ("n", "m2", 25, Some("4")),
        ("q", "m1", 50, Some("2")),
        ("r", "m2", 20, Some("3")),
        ("s", "m3", 5, Some("0.0")),
        ("t", "m3", 6, Some("0.3")),
        ("y", "m2", 40, Some("8")),
        ("z", "m1", 60, Some("5")),
        ("za", "m4", 7, None),
    ] {
        let n = n.map_or_else(String::new, |n| format!(", n: {n}"));
        script.push_str(&format!(
            "CREATE readings:'{id}' = {{ meter: '{meter}', \
             at: datetime '2026-10-07T10:{:02}:{:02}Z'{n} }};\n",
            second / 60,
            second % 60,
        ));
    }
    signed_in(&leader, "root").run(&script).unwrap();
    signed_in(&leader, "root")
        .run(
            "USE NAMESPACE prod; USE DATABASE shop; \
             GRANT read ON readings FIELDS meter, at TO narrow;",
        )
        .unwrap();
    let follower = follower_of_the_middle_of(&leader, "readings");
    pair_of(leader, follower)
}

/// One read per fixture C2 names; grouped by meter, so a group is a fixture.
const HOLDING: [&str; 6] = [
    // An even count (m1, m2), floats (m3), and a group with nothing in it (m4).
    "SELECT meter, median(n) AS middle FROM readings GROUP BY meter;",
    // Key order across the shards, and the empty array for m4.
    "SELECT meter, collect(n) AS every FROM readings GROUP BY meter;",
    "SELECT collect(at) AS times FROM readings;",
    // A reset across a shard boundary (m1), interleaved samples (m2), floats (m3).
    "SELECT meter, increase(n, at) AS up, rate(n, at) AS per, delta(n, at) AS change \
     FROM readings GROUP BY meter;",
    // Beside a fold that held a constant all along.
    "SELECT count(*) AS c, median(n) AS m, collect(meter) AS who FROM readings WHERE n > 2;",
    "SELECT meter, increase(n, at) AS up FROM readings WHERE meter = 'm1' GROUP BY meter;",
];

/// ADR-0121 — each holding fold is folded on the leaders, so no record of the
/// shards this node lacks travels, and the answer is still the whole node's.
#[test]
fn a_holding_fold_is_folded_on_the_leaders_and_no_record_travels() {
    let pair = readings();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in HOLDING {
        let (expected, _) = answer(&mut whole, read);
        assert!(!expected.is_empty(), "{read}: the control answered nothing");
        let (gathered, notes) = answer(&mut follower, read);
        assert_eq!(gathered, expected, "{read}");
        assert_eq!(pair.sent(), 0, "{read}: records travelled");
        assert!(
            notes.iter().any(|note| note.kind() == "gathered"),
            "{read}: {notes:?}"
        );
    }
}

/// A redacted field is absent on the leaders exactly as it is in a walk: the
/// folds over `n`, which `narrow` may not read, fold nothing.
#[test]
fn a_holding_fold_over_a_field_the_reader_may_not_see_folds_nothing() {
    let pair = readings();
    let mut follower = pair.on_the_follower("narrow");
    let mut whole = pair.on_the_leader("narrow");
    let read = "SELECT meter, collect(n) AS every, median(n) AS middle, \
                increase(n, at) AS up, collect(at) AS times FROM readings GROUP BY meter;";
    let (expected, _) = answer(&mut whole, read);
    assert_eq!(expected.len(), 4, "{expected:?}");
    assert!(
        !format!("{expected:?}").contains("Float"),
        "the control read `n`: {expected:?}"
    );
    assert_eq!(answer(&mut follower, read).0, expected);
    assert_eq!(pair.sent(), 0, "records travelled");
}

/// The meter's row from a grouped answer.
fn row<'a>(rows: &'a [(RecordId, Value)], meter: &str) -> &'a BTreeMap<String, Value> {
    rows.iter()
        .find_map(|(_, row)| match row {
            Value::Object(fields) if fields.get("meter") == Some(&Value::from(meter)) => {
                Some(fields)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {meter} in {rows:?}"))
}

/// ADR-0121 D3, counted by hand: summaries that follow one another in time
/// are joined with the rise across each seam — a reset included — and samples
/// that interleave are asked for again, once, as samples.
#[test]
fn a_counter_reset_across_a_shard_boundary_and_interleaved_samples_count_as_one_walk() {
    let pair = readings();
    let mut follower = pair.on_the_follower("reader");
    pair.asked();
    // m1 alone: three shards in time order, so one asking of each of the two
    // shards this node lacks.
    let (rows, _) = answer(
        &mut follower,
        "SELECT meter, increase(n, at) AS up, delta(n, at) AS change, rate(n, at) AS per \
         FROM readings WHERE meter = 'm1' GROUP BY meter;",
    );
    let m1 = row(&rows, "m1");
    // 1, 4 | 6, 9 | 2, 5: rises 3, 2 across the seam, 3, a reset to 2 across
    // the next seam, then 3 — over the fifty seconds from 10 s to 60 s.
    assert_eq!(m1.get("up"), Some(&Value::from(13_i64)), "{m1:?}");
    assert_eq!(m1.get("change"), Some(&Value::from(4_i64)), "{m1:?}");
    assert_eq!(
        m1.get("per"),
        Some(&Value::Number(Number::Decimal("0.26".parse().unwrap()))),
        "{m1:?}"
    );
    assert_eq!(pair.asked().len(), 2, "m1 chains and is asked once");
    // m2's shards interleave: 1 at 10 s, 5 at 30 s | 4 at 25 s | 3 at 20 s,
    // 8 at 40 s. In time order 1, 3, 4, 5, 8: rises 2, 1, 1, 3.
    let (rows, _) = answer(
        &mut follower,
        "SELECT meter, increase(n, at) AS up, delta(n, at) AS change \
         FROM readings WHERE meter = 'm2' GROUP BY meter;",
    );
    let m2 = row(&rows, "m2");
    assert_eq!(m2.get("up"), Some(&Value::from(7_i64)), "{m2:?}");
    assert_eq!(m2.get("change"), Some(&Value::from(7_i64)), "{m2:?}");
    assert_eq!(pair.asked().len(), 4, "m2 is asked again, for its samples");
    assert_eq!(pair.sent(), 0, "records travelled");
    // m3's rises 0.1, 0.2 and 0.3, summed exactly and rounded once (D4).
    let (rows, _) = answer(
        &mut follower,
        "SELECT meter, increase(n, at) AS up FROM readings WHERE meter = 'm3' GROUP BY meter;",
    );
    assert_eq!(
        row(&rows, "m3").get("up"),
        Some(&Value::Number(Number::float(0.6)))
    );
}

/// Leaders folding shards of `GATHER_RECORDS` records each, every one of them
/// the value `1`.
#[derive(Debug, Default)]
struct HoldingMany {
    asked: AtomicUsize,
}

impl Gather for HoldingMany {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        let many = i64::try_from(GATHER_RECORDS).unwrap();
        let states = asked
            .reduce
            .unwrap()
            .folds
            .iter()
            .map(|folded| match folded.fold.spelling() {
                "median" => Value::Array(vec![Value::Array(vec![
                    Value::Number(Number::Decimal(1.into())),
                    Value::from(many),
                ])]),
                "collect" => Value::Array(vec![Value::from(1_i64); GATHER_RECORDS]),
                other => panic!("{other}"),
            })
            .collect();
        Ok(Gathered {
            records: Vec::new(),
            node: [9; 16],
            reduced: Some(Reduced::Partials(vec![Partial {
                key: Vec::new(),
                first: RecordId::from("a"),
                states,
            }])),
            counted: None,
        })
    }
}

/// ADR-0121 D5 — the ceiling counts what travels: a `median` over more records
/// than a gather may hold answers when its distinct values fit, and a
/// `collect`, whose answer is its values, is still refused past it.
#[test]
fn a_median_over_more_records_than_a_gather_holds_answers_and_a_collect_is_refused() {
    let pair = pair();
    let leaders = Arc::new(HoldingMany::default());
    let mut follower =
        signed_in(pair.follower(), "reader").gathering(Arc::clone(&leaders) as Arc<dyn Gather>);
    follower
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    // Twice `GATHER_RECORDS` ones, beside this node's own 2 and 7.
    let (rows, _) = answer(&mut follower, "SELECT median(total) AS middle FROM ledger;");
    let (expected, _) = answer(
        &mut signed_in(pair.leader(), "root"),
        "USE NAMESPACE prod; USE DATABASE shop; DEFINE COLLECTION ones;\n\
         CREATE ones = { x: 1 }; CREATE ones = { x: 1 }; CREATE ones = { x: 1 };\n\
         CREATE ones = { x: 2 }; CREATE ones = { x: 7 };\n\
         SELECT median(x) AS middle FROM ones;",
    );
    assert_eq!(rows[0].1, expected[0].1, "{rows:?}");
    assert_eq!(leaders.asked.load(Ordering::Relaxed), 2);
    match refused(&mut follower, "SELECT collect(total) AS every FROM ledger;") {
        tessari_session::Error::GatheredTooMuch { table, .. } => assert_eq!(table, "ledger"),
        other => panic!("expected GatheredTooMuch, got {other:?}"),
    }
}
