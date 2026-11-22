//! A judgment set over the full-text scorer (G095 C5).
//!
//! The SG1 study code-verified the scorer's constants as canonical BM25
//! (`rank.rs`: the floored idf, k1 = 1.2, b = 0.75). What a constant check
//! cannot do is watch the scorer rank realistic text — stemming, term
//! saturation and length normalization interacting — so this module holds a
//! small hand-graded corpus and a second, independent BM25 implementation,
//! and pins the store's ordering against it. The verdict C5 records is the
//! keep-decision (ADR-0141 D5): the run is the evidence, and a defect would
//! be an ordering the independent implementation cannot reproduce.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use tessari_session::RunNow;

use tessari_session::Session;
use tessari_types::RecordId;

use super::key_value::{on_each_backend, opened, run};

/// Ten short documents on one small vocabulary, written so the scorer's three
/// levers all bite: `sail` repeats (saturation), one long document (length
/// normalization), and `row`/`rowing` forms (stemming).
const DOCS: &[(&str, &str)] = &[
    (
        "d01",
        "A sail catches the wind. A sail boat moves when the wind fills the sail.",
    ),
    (
        "d02",
        "Boat builders shape wood. Wood planks bend. A wooden boat takes shape.",
    ),
    (
        "d03",
        "A rower rows with oars. Oar locks hold the oars steady.",
    ),
    (
        "d04",
        "The hull of a boat keeps water out. Hull repair is honest work.",
    ),
    (
        "d05",
        "A fast sail boat sails fast when the wind is strong and the water is flat.",
    ),
    (
        "d06",
        "Wood floats. Wood is the old material for boats of every size.",
    ),
    (
        "d07",
        "Wind over open water drives a sail harder than still air ever does.",
    ),
    (
        "d08",
        "A long lecture on boats: a boat is a vessel, vessels float, boats carry people, people row boats, sails push boats, the hull keeps the sea outside, and every boat whatever its size meets the water at the hull first.",
    ),
    (
        "d09",
        "To row is to move a boat with oars. Row hard, row steady, row home.",
    ),
    ("d10", "The lonely anchor waits on the bottom."),
];

/// Queries with graded judgments: 2 = squarely relevant, 1 — useful, 0 — not.
const JUDGMENTS: &[(&str, &[(&str, u8)])] = &[
    (
        "sail boat",
        &[("d01", 2), ("d05", 2), ("d07", 1), ("d02", 1), ("d08", 1)],
    ),
    ("row", &[("d09", 2), ("d03", 1), ("d08", 1)]),
    ("hull", &[("d04", 2), ("d08", 1)]),
    ("fast wind", &[("d05", 2), ("d07", 1)]),
];

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// The independent implementation: canonical BM25 over a tokenized corpus,
/// mirroring the formula the engine documents rather than the engine's code.
struct Golden {
    /// doc -> term -> count.
    counts: BTreeMap<&'static str, BTreeMap<String, u32>>,
    lengths: BTreeMap<&'static str, usize>,
    average: f64,
}

impl Golden {
    fn new() -> Self {
        let mut counts = BTreeMap::new();
        let mut lengths = BTreeMap::new();
        for (id, text) in DOCS {
            let mut terms = BTreeMap::new();
            for word in text.split(|c: char| !c.is_alphanumeric()) {
                let term = stem(word.to_lowercase());
                if !term.is_empty() {
                    let held = terms.entry(term).or_insert(0_u32);
                    *held = held.saturating_add(1);
                }
            }
            lengths.insert(
                *id,
                terms
                    .values()
                    .map(|n| usize::try_from(*n).unwrap_or(0))
                    .sum(),
            );
            counts.insert(*id, terms);
        }
        let as_float = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
        let average = lengths.values().map(|n| as_float(*n)).sum::<f64>() / as_float(lengths.len());
        Self {
            counts,
            lengths,
            average,
        }
    }

    /// The floored idf the scorer documents: ln(1 + (N − df + 0.5) / (df + 0.5)).
    fn idf(&self, term: &str) -> f64 {
        let docs = f64::from(u32::try_from(self.counts.len()).unwrap_or(u32::MAX));
        let df = self
            .counts
            .values()
            .filter(|held| held.contains_key(term))
            .count();
        let df = f64::from(u32::try_from(df).unwrap_or(u32::MAX));
        (1.0 + (docs - df + 0.5) / (df + 0.5)).ln()
    }

    fn score(&self, id: &str, query: &str) -> f64 {
        let Some(held) = self.counts.get(id) else {
            return 0.0;
        };
        let length =
            f64::from(u32::try_from(*self.lengths.get(id).unwrap_or(&0)).unwrap_or(u32::MAX));
        query
            .split(|c: char| !c.is_alphanumeric())
            .filter_map(|word| {
                let term = stem(word.to_lowercase());
                let frequency = f64::from(*held.get(&term)?);
                Some(
                    self.idf(&term) * frequency * (K1 + 1.0)
                        / (frequency + K1 * (1.0 - B + B * length / self.average)),
                )
            })
            .sum()
    }

    /// The golden order: every document of the corpus, by the score — a
    /// single `ORDER BY` answers the whole table, so the documents no term
    /// scored ride at the tail in identity order, exactly as the store's own
    /// order rule ends every sort.
    fn order(&self, query: &str) -> Vec<RecordId> {
        let mut scored: Vec<(f64, RecordId)> = self
            .counts
            .keys()
            .map(|id| (self.score(id, query), RecordId::from(*id)))
            .collect();
        scored.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        scored.into_iter().map(|(_, id)| id).collect()
    }
}

/// The one stem this corpus needs, matching the english analyzer's Porter2
/// output for the forms the documents use. Kept as a table rather than a
/// stemmer: the corpus is fixed, and a table cannot drift the way a
/// reimplemented stemmer can.
fn stem(word: String) -> String {
    match word.as_str() {
        "rows" | "rowed" | "rowing" => "row".to_owned(),
        "oars" => "oar".to_owned(),
        "sails" | "sailed" | "sailing" => "sail".to_owned(),
        "boats" => "boat".to_owned(),
        "hulls" => "hull".to_owned(),
        "winds" => "wind".to_owned(),
        other => other.to_owned(),
    }
}

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

/// NDCG@10 of `order` under `judged`: gains 2/1 by grade, log-discounted.
fn ndcg(order: &[RecordId], judged: &[(&str, u8)]) -> f64 {
    let gain = |id: &RecordId| {
        judged
            .iter()
            .find(|(name, _)| RecordId::from(*name) == *id)
            .map_or(0.0, |(_, grade)| f64::from(*grade))
    };
    let dcg = |ranked: &[RecordId]| {
        ranked
            .iter()
            .take(10)
            .enumerate()
            .map(|(place, id)| {
                gain(id) / (f64::from(u32::try_from(place).unwrap_or(u32::MAX)) + 2.0).log2()
            })
            .sum::<f64>()
    };
    let mut ideal: Vec<RecordId> = judged
        .iter()
        .filter(|(_, grade)| *grade > 0)
        .map(|(name, _)| RecordId::from(*name))
        .collect();
    ideal.sort_by(|left, right| {
        gain(right)
            .total_cmp(&gain(left))
            .then_with(|| left.cmp(right))
    });
    let ideal = dcg(&ideal);
    if ideal == 0.0 {
        1.0
    } else {
        dcg(order) / ideal
    }
}

#[test]
fn the_scorer_ranks_the_judgment_set_as_bm25_does() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let mut script = String::from(
            "DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE TABLE boats (text string ANALYZER english);\n\
             DEFINE INDEX by_text ON boats FIELDS text SEARCH;",
        );
        for (id, text) in DOCS {
            script.push_str(&format!(" CREATE boats:'{id}' = {{ text: '{text}' }};"));
        }
        run(&mut session, &script);

        let golden = Golden::new();
        let mut ndcgs = Vec::new();
        for (query, judged) in JUDGMENTS {
            let read =
                format!("SELECT id FROM boats ORDER BY search::score(text, '{query}') DESC, id;");
            let answered = ids(&mut session, &read);
            let expected = golden.order(query);
            assert_eq!(
                answered, expected,
                "{}: '{query}' ranked differently from canonical BM25: {answered:?} != {expected:?}",
                backend.name
            );
            // The distractor nothing scored rides last — a score of zero is
            // an order, not an absence, in a single `ORDER BY`.
            assert_eq!(
                answered.last(),
                Some(&RecordId::from("d10")),
                "{}: the unscored distractor did not ride last on '{query}'",
                backend.name
            );
            ndcgs.push((query, ndcg(&answered, judged)));
        }
        let worst = ndcgs.iter().map(|(_, held)| *held).fold(1.0_f64, f64::min);
        assert!(
            worst >= 0.85,
            "{}: the judgment set's worst NDCG@10 is {worst:.3}: {ndcgs:?}",
            backend.name
        );
    });
}
