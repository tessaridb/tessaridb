//! `ORDER BY FUSE (…)` answers the reciprocal-rank fusion of its branches (G038).
//!
//! The expectation is computed here, independently: each branch's order comes
//! from the store's own single-key `ORDER BY` — a path with its own tests — and
//! the fusion arithmetic is done in this file. So the fused read is checked
//! against a second implementation, never against itself.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use tessari_session::Session;
use tessari_types::{Number, RecordId, Value};

use super::key_value::{on_each_backend, opened, run};

const K: f64 = 60.0;

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    session
        .run(read)
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
            .run(
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
            let why = session.run(read).unwrap_err().to_string();
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
