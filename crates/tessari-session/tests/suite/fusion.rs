//! `ORDER BY FUSE (…)` answers the reciprocal-rank fusion of its branches (G038).
//!
//! The expectation is computed here, independently: each branch's order comes
//! from the store's own single-key `ORDER BY` — a path with its own tests — and
//! the fusion arithmetic is done in this file. So the fused read is checked
//! against a second implementation, never against itself.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use tessari_session::RunNow;

use tessari_session::Session;
use tessari_types::{Number, RecordId, Value};

use super::key_value::{on_each_backend, opened, run};

const K: f64 = 60.0;

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    session
        .run_now(read)
        .unwrap()
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

/// Sixty records with ties in `a`, absences in `b`, and a filter on `c`.
fn corpus(session: &mut Session<'_>) {
    let mut script = String::from("DEFINE COLLECTION t;");
    for n in 0..60_u32 {
        let b = if n % 9 == 0 {
            String::new()
        } else {
            format!(", b: {}", f64::from(n.saturating_mul(37) % 11) / 3.0)
        };
        script.push_str(&format!(
            " CREATE t:'r{n:02}' = {{ a: {}, c: {}{b} }};",
            n % 7,
            n % 3
        ));
    }
    run(session, &script);
}

/// The fusion of `branches` (each an `ORDER BY` clause body and a weight),
/// computed from the store's own single-key orders.
fn expected(session: &mut Session<'_>, branches: &[(&str, f64)], depth: usize) -> Vec<RecordId> {
    let mut scores: BTreeMap<RecordId, f64> = BTreeMap::new();
    for (key, weight) in branches {
        let order = ids(
            session,
            &format!("SELECT * FROM t WHERE c != 0 ORDER BY {key};"),
        );
        for (place, id) in order.into_iter().take(depth).enumerate() {
            let rank = f64::from(u32::try_from(place).unwrap().saturating_add(1));
            *scores.entry(id).or_default() += weight / (K + rank);
        }
    }
    let mut ranked: Vec<(RecordId, f64)> = scores.into_iter().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.into_iter().map(|(id, _)| id).collect()
}

#[test]
fn a_fused_read_answers_the_rank_fusion_of_its_branches_at_every_cut() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        for (branches, depth, clause) in [
            (
                vec![("a DESC", 1.0), ("b", 2.0)],
                15,
                "FUSE (a DESC, b WEIGHT 2) DEPTH 15",
            ),
            (
                vec![("a", 1.0), ("b DESC", 0.5), ("c DESC", 1.0)],
                100,
                "FUSE (a, b DESC WEIGHT 0.5, c DESC)",
            ),
        ] {
            let whole = expected(&mut session, &branches, depth);
            assert!(
                whole.len() > 10,
                "{}: the corpus proves nothing",
                backend.name
            );
            let read = format!("SELECT * FROM t WHERE c != 0 ORDER BY {clause}");
            assert_eq!(
                ids(&mut session, &format!("{read};")),
                whole,
                "{} {clause}",
                backend.name
            );
            for (start, limit) in [(0, 1), (0, 5), (3, 4), (whole.len() - 2, 10)] {
                let cut: Vec<RecordId> = whole.iter().skip(start).take(limit).cloned().collect();
                assert_eq!(
                    ids(
                        &mut session,
                        &format!("{read} START {start} LIMIT {limit};")
                    ),
                    cut,
                    "{} {clause} START {start} LIMIT {limit}",
                    backend.name
                );
            }
        }
    });
}

#[test]
fn a_record_no_branch_ranks_within_its_depth_is_not_answered() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        let answered = ids(
            &mut session,
            "SELECT * FROM t WHERE c != 0 ORDER BY FUSE (a DESC, b) DEPTH 3;",
        );
        assert!(
            answered.len() <= 6 && answered.len() >= 3,
            "{}: {answered:?}",
            backend.name
        );
    });
}

#[test]
fn records_the_fusion_cannot_tell_apart_are_ordered_by_identity() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        // `p` is first by `a` and second by `b`, `q` the other way round, so the
        // two fused sums are equal and only the identity can order them.
        run(
            &mut session,
            "DEFINE COLLECTION t; CREATE t:'q' = { a: 1, b: 1 }; CREATE t:'p' = { a: 2, b: 2 };",
        );
        let answered = ids(&mut session, "SELECT * FROM t ORDER BY FUSE (a DESC, b);");
        assert_eq!(
            answered,
            vec![RecordId::from("p"), RecordId::from("q")],
            "{}",
            backend.name
        );
    });
}

#[test]
fn search_ranks_answers_where_each_branch_placed_the_record() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        let placed = |session: &mut Session<'_>, key: &str| -> BTreeMap<RecordId, i64> {
            ids(
                session,
                &format!("SELECT * FROM t WHERE c != 0 ORDER BY {key};"),
            )
            .into_iter()
            .take(15)
            .zip(1_i64..)
            .collect()
        };
        let (by_a, by_b) = (placed(&mut session, "a DESC"), placed(&mut session, "b"));
        let answered = session
            .run_now(
                "SELECT search::ranks() AS ranks FROM t WHERE c != 0 \
                 ORDER BY FUSE (a DESC, b) DEPTH 15;",
            )
            .unwrap()
            .last()
            .unwrap()
            .records()
            .unwrap()
            .to_vec();
        assert!(answered.len() > 10, "{}", backend.name);
        for (id, record) in answered {
            let Value::Object(fields) = record else {
                panic!()
            };
            let rank = |held: Option<&i64>| {
                held.map_or(Value::None, |at| Value::Number(Number::Integer(*at)))
            };
            assert_eq!(
                fields.get("ranks"),
                Some(&Value::Array(vec![
                    rank(by_a.get(&id)),
                    rank(by_b.get(&id))
                ])),
                "{} {id}",
                backend.name
            );
        }
    });
}

#[test]
fn search_ranks_outside_a_fused_read_is_refused_by_name() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        for read in [
            "SELECT search::ranks() AS r FROM t;",
            "SELECT search::ranks() AS r FROM t ORDER BY a LIMIT 3;",
        ] {
            let why = session.run_now(read).unwrap_err().to_string();
            assert!(
                why.contains(
                    "search::ranks() answers only in the projection of a read ordered by FUSE"
                ),
                "{} {read}: {why}",
                backend.name
            );
        }
    });
}

/// Text, vector and place in one fused read (S4.1). Record 1 is first in every
/// branch; 3 beats 2 only because two of three branches place it second —
/// `1/63 + 2/62` against `1/62 + 2/63`.
#[test]
fn text_vector_and_place_fuse_in_one_read() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE ANALYZER plain FILTERS lowercase;\n\
             DEFINE TABLE docs SCHEMALESS;\n\
             DEFINE FIELD body ON docs TYPE string ANALYZER plain;\n\
             DEFINE INDEX by_body ON docs FIELDS body SEARCH;\n\
             CREATE docs:1 = { body: 'lock contention and a lock', e: [1, 0], \
               at: geometry { type: 'Point', coordinates: [0, 0] } };\n\
             CREATE docs:2 = { body: 'a lock', e: [0, 1], \
               at: geometry { type: 'Point', coordinates: [10, 10] } };\n\
             CREATE docs:3 = { body: 'nothing to see', e: [0.9, 0.1], \
               at: geometry { type: 'Point', coordinates: [0, 1] } };",
        );
        let answered = ids(
            &mut session,
            "SELECT * FROM docs ORDER BY FUSE (\
               search::score(body, 'lock contention') DESC, \
               vector::cosine(e, [1, 0]), \
               geo::distance(at, geometry { type: 'Point', coordinates: [0, 0] }));",
        );
        assert_eq!(
            answered,
            [1, 3, 2].map(RecordId::from).to_vec(),
            "{}",
            backend.name
        );
    });
}

/// The golden bounds of the fused-order grammar (T1.4): one branch is not a
/// fusion, a weight must be above zero, a depth must be a whole number above
/// zero, and `FUSE` is the whole order.
#[test]
fn the_fused_order_refuses_its_out_of_range_shapes() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        for read in [
            "SELECT * FROM t ORDER BY FUSE (a DESC);",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b WEIGHT 0);",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b WEIGHT -2);",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b WEIGHT 1.5e400);",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b) DEPTH 0;",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b) DEPTH 1.5;",
            "SELECT * FROM t ORDER BY FUSE (a DESC, b), a;",
        ] {
            assert!(
                session.run_now(read).is_err(),
                "{}: {read} answered",
                backend.name
            );
        }
    });
}

/// `DEPTH 1` counts only each branch's first record: the golden reads the
/// answer off the store's own single-key orders, computed here.
#[test]
fn depth_one_counts_only_each_branchs_first_record() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        let whole = expected(
            &mut session,
            &[("a DESC", 1.0), ("b", 2.0), ("c DESC", 1.0)],
            1,
        );
        assert_eq!(whole.len(), 3, "{}: three firsts", backend.name);
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM t WHERE c != 0 ORDER BY FUSE (a DESC, b WEIGHT 2, c DESC) DEPTH 1;"
            ),
            whole,
            "{}",
            backend.name
        );
    });
}

/// A weight names how much an order counts (T1.4): doubling one branch moves
/// the records only that branch placed, and the golden answer is computed
/// from the store's own single-key orders with the weight applied here.
#[test]
fn a_weight_names_how_much_an_order_counts() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        corpus(&mut session);
        let branches = &[("a DESC", 3.0), ("b", 1.0)];
        let golden = expected(&mut session, branches, 15);
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM t WHERE c != 0 ORDER BY FUSE (a DESC WEIGHT 3, b) DEPTH 15;"
            ),
            golden,
            "{}",
            backend.name
        );
        // The weights are not symmetric: swapping them re-ranks the middle.
        let swapped = expected(&mut session, &[("a DESC", 1.0), ("b", 3.0)], 15);
        assert_ne!(
            golden, swapped,
            "{}: the corpus must distinguish the weights",
            backend.name
        );
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM t WHERE c != 0 ORDER BY FUSE (a DESC, b WEIGHT 3) DEPTH 15;"
            ),
            swapped,
            "{}",
            backend.name
        );
    });
}

/// Text and vector in one fusion (T1.4's two-branch case): a search-score
/// branch and a distance branch, each defined by the store's own single-key
/// order — the search index serving the one, the vector index the other. A
/// record the vector branch cannot place (no vector) still answers when the
/// text branch places it, and is golden-computed the same way.
#[test]
fn a_fusion_of_text_and_vector_places_what_either_branch_places() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;
             DEFINE TABLE docs (body string ANALYZER english, embedding vector<2>);
             DEFINE INDEX body ON docs FIELDS body SEARCH;
             DEFINE INDEX embedding ON docs FIELDS embedding VECTOR euclidean;
             CREATE docs:'a' = { body: 'the quick fox jumps', embedding: [0.0, 0.0] };
             CREATE docs:'b' = { body: 'a lazy dog sleeps', embedding: [0.0, 10.0] };
             CREATE docs:'c' = { body: 'the fox meets the dog', embedding: [10.0, 10.0] };
             CREATE docs:'d' = { body: 'quick quick quick' };
             CREATE docs:'e' = { body: 'nothing to see here', embedding: [7.0, 7.0] };",
        );
        let text = ids(
            &mut session,
            "SELECT * FROM docs ORDER BY search::score(body, 'quick fox') DESC;",
        );
        let vector = ids(
            &mut session,
            "SELECT * FROM docs ORDER BY vector::euclidean(embedding, [0.0, 1.0]);",
        );
        // The golden, computed from the two single-key orders above: each
        // branch's first `depth` ranks earn weight / (60 + rank).
        let mut scores: BTreeMap<RecordId, f64> = BTreeMap::new();
        for (order, weight) in [(&text, 2.0), (&vector, 1.0)] {
            for (place, id) in order.iter().take(4).enumerate() {
                let rank = f64::from(u32::try_from(place).unwrap().saturating_add(1));
                *scores.entry(id.clone()).or_default() += weight / (K + rank);
            }
        }
        let mut golden: Vec<RecordId> = scores.into_keys().collect();
        golden.sort_by(|left, right| {
            let score = |id: &RecordId| {
                let mut held = 0.0;
                for (order, weight) in [(&text, 2.0), (&vector, 1.0)] {
                    if let Some(place) = order.iter().position(|at| at == id)
                        && place < 4
                    {
                        held += weight
                            / (K + f64::from(u32::try_from(place).unwrap().saturating_add(1)));
                    }
                }
                held
            };
            score(right)
                .total_cmp(&score(left))
                .then_with(|| left.cmp(right))
        });
        assert!(
            golden.contains(&RecordId::from("d")),
            "{}: the unvectorized record the text branch placed is missing from the golden",
            backend.name
        );
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM docs ORDER BY FUSE (search::score(body, 'quick fox') DESC WEIGHT 2, vector::euclidean(embedding, [0.0, 1.0])) DEPTH 4;"
            ),
            golden,
            "{}",
            backend.name
        );
    });
}
