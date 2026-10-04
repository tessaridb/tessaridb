#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;

use tessari_types::{Analyzer, Number, Value};

use super::{Corpus, Held, TermStatistics, bound, saturation, score};

fn analyzer() -> Analyzer {
    Analyzer::default()
}

fn corpus(documents: u64, average_length: f64, frequencies: &[(&str, u64)]) -> Corpus {
    Corpus {
        documents,
        average_length,
        terms: frequencies
            .iter()
            .map(|(term, held)| ((*term).to_owned(), TermStatistics::new(*held)))
            .collect(),
        asked: Vec::new(),
        blends: Vec::new(),
    }
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Number(Number::Float(held)) => *held,
        other => panic!("not a score: {other:?}"),
    }
}

/// Score this text against this query, the way a read with no index payload
/// does it: the query analysed once, the record's numbers taken from its own
/// text.
fn scored(corpus: &Corpus, text: &str, query: &str) -> f64 {
    let asked = analyzer().terms(query);
    let held = Held::analysed(&analyzer(), text, &asked);
    let corpus = Corpus {
        asked,
        blends: Vec::new(),
        ..corpus.clone()
    };
    number(&score(&corpus, &held))
}

/// The same score, with the record's two numbers supplied the way a posting
/// supplies them.
fn posted(corpus: &Corpus, occurrences: &[(&str, u32)], length: u32, query: &str) -> f64 {
    let counted: BTreeMap<String, u32> = occurrences
        .iter()
        .map(|(term, held)| ((*term).to_owned(), *held))
        .collect();
    let corpus = Corpus {
        asked: analyzer().terms(query),
        blends: Vec::new(),
        ..corpus.clone()
    };
    number(&score(&corpus, &Held::counted(counted, length)))
}

#[test]
fn holding_more_of_the_query_outranks_holding_less() {
    let corpus = corpus(100, 10.0, &[("lock", 20), ("contention", 5)]);
    let both = scored(&corpus, "lock contention here", "lock contention");
    let one = scored(&corpus, "lock here", "lock contention");
    assert!(both > one, "{both} vs {one}");
    assert!(one > 0.0);
}

#[test]
fn a_rare_term_is_worth_more_than_a_common_one() {
    // The whole reason a score is not a count of matches. Two documents, one
    // word each, differing only in how many other documents hold that word.
    let corpus = corpus(1_000, 10.0, &[("rare", 2), ("common", 900)]);
    let scarce = scored(&corpus, "rare", "rare");
    let usual = scored(&corpus, "common", "common");
    assert!(scarce > usual, "{scarce} vs {usual}");
}

#[test]
fn a_term_in_most_of_the_collection_still_counts_for_something() {
    // Textbook BM25 goes negative here, which would rank a document holding
    // the word *below* one that does not hold it at all.
    let corpus = corpus(100, 10.0, &[("the", 99)]);
    let held = scored(&corpus, "the", "the");
    assert!(held > 0.0, "{held}");
}

#[test]
fn a_short_document_outranks_a_long_one_holding_the_term_as_often() {
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    let short = scored(&corpus, "lock", "lock");
    let long = scored(
        &corpus,
        "lock and a great deal of other prose about entirely unrelated matters",
        "lock",
    );
    assert!(short > long, "{short} vs {long}");
}

#[test]
fn repetition_helps_and_then_stops_helping() {
    // `k1` is what makes the tenth occurrence worth less than the second;
    // without saturation a document could be lifted by repeating one word.
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    let once = scored(&corpus, "lock", "lock");
    let twice = scored(&corpus, "lock lock", "lock");
    let many = scored(&corpus, "lock lock lock lock lock lock lock lock", "lock");
    assert!(twice > once);
    assert!(many > twice);
    assert!(many - twice < twice - once, "saturation is not saturating");
}

#[test]
fn a_record_holding_none_of_the_query_scores_zero() {
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    assert_eq!(scored(&corpus, "something else entirely", "lock"), 0.0);
    assert_eq!(scored(&corpus, "", "lock"), 0.0);
}

#[test]
fn an_empty_collection_scores_every_record_the_same() {
    // Which is the true answer rather than a safe one: with nothing to
    // compare against, no record answers the query better than another.
    let corpus = corpus(0, 0.0, &[]);
    assert_eq!(scored(&corpus, "lock contention", "lock"), 0.0);
}

#[test]
fn a_record_the_index_posts_nothing_for_scores_zero() {
    // Which is what a value that is not text reaches: nothing was analysed,
    // so nothing was posted, so the query's words are held nowhere. The same
    // answer re-reading the record would give, arrived at without reading it.
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    assert_eq!(posted(&corpus, &[], 0, "lock"), 0.0);
}

#[test]
fn the_two_ways_of_supplying_a_records_numbers_agree() {
    // F2's assertion at the smallest scale it can be made: the postings
    // carry what the analysis would have counted, so a score computed from
    // them is not merely close to the one computed from the text — it is the
    // same float. A test asserting "roughly equal" here would pass on an
    // implementation that had quietly changed the ranking.
    let corpus = corpus(1_000, 12.0, &[("lock", 20), ("contention", 5)]);
    let text = "lock contention and more lock contention in a long enough line";
    let query = "lock contention";
    let from_text = scored(&corpus, text, query);
    let from_index = posted(
        &corpus,
        &[("lock", 2), ("contention", 2)],
        u32::try_from(analyzer().terms(text).len()).unwrap(),
        query,
    );
    assert!(from_text > 0.0, "{from_text}");
    assert!(
        (from_text - from_index).abs() < f64::EPSILON,
        "{from_text} vs {from_index}"
    );
}

#[test]
fn a_word_written_twice_in_a_query_weighs_twice() {
    // The reason the asked terms travel as a multiset. Deduplicating them
    // would not rescale the answers: it would lower the records holding the
    // repeated word and leave the others where they are, which is a
    // reordering wearing a simplification's clothes.
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    let once = scored(&corpus, "lock and other prose", "lock");
    let twice = scored(&corpus, "lock and other prose", "lock lock");
    assert!(
        (twice - 2.0 * once).abs() < f64::EPSILON,
        "{twice} vs {once}"
    );
}

#[test]
fn a_query_term_nobody_holds_lifts_nothing() {
    let corpus = corpus(100, 10.0, &[("lock", 10)]);
    let with = scored(&corpus, "lock", "lock unheardof");
    let without = scored(&corpus, "lock", "lock");
    assert!((with - without).abs() < f64::EPSILON, "{with} vs {without}");
}

#[test]
fn the_frequencies_map_missing_a_term_treats_it_as_unseen() {
    // A resolution that failed to count a term must not silently score it as
    // if every document held it; an unlisted term is one no document holds.
    let corpus = corpus(100, 10.0, &[]);
    assert!(scored(&corpus, "lock", "lock") > 0.0);
}

/// **The property a pruning bound rests on** (ADR-0050): the saturation term
/// is increasing in occurrences and decreasing in length.
///
/// Asserted here rather than assumed, for the reason the phrase tests assert
/// the filter chain is one token in and one token out — a later change to
/// the scoring function that broke either direction would not fail a scoring
/// test, because every score it produced would still be a perfectly ordinary
/// number. It would fail by removing records from a pruned answer, in a
/// completely different crate, with nothing in an error state.
///
/// Both directions are needed and neither implies the other, because the
/// bound pairs the largest frequency with the smallest length — two numbers
/// that usually come from two different records.
#[test]
fn saturation_rises_with_occurrences_and_falls_with_length() {
    for average in [0.5, 1.0, 5.0, 50.0, 500.0, 5000.0] {
        for length in [1.0, 3.0, 17.0, 240.0] {
            for occurrences in [1.0, 2.0, 9.0, 40.0] {
                assert!(
                    saturation(occurrences + 1.0, length, average)
                        > saturation(occurrences, length, average),
                    "one more occurrence scored no higher at \
                         length {length}, average {average}"
                );
                assert!(
                    saturation(occurrences, length + 1.0, average)
                        < saturation(occurrences, length, average),
                    "a longer record scored no lower at \
                         {occurrences} occurrences, average {average}"
                );
            }
        }
    }
}

/// And therefore the extremes dominate every posting, at any average.
///
/// The composition of the two directions above, stated as the thing a
/// pruning evaluator will actually rely on: pairing the largest frequency
/// with the smallest length bounds a term's whole posting list, whatever the
/// collection's average length happens to be *now* — which is precisely what
/// a stored impact could not promise, because it was computed at one average
/// and read at another.
#[test]
fn the_extremes_bound_every_posting_at_every_average() {
    let postings = [(3.0, 40.0), (12.0, 900.0), (1.0, 2.0), (7.0, 55.0)];
    let most = postings.iter().fold(0.0_f64, |held, (f, _)| held.max(*f));
    let fewest = postings
        .iter()
        .fold(f64::INFINITY, |held, (_, dl)| held.min(*dl));

    for average in [0.5, 1.0, 5.0, 50.0, 500.0, 5000.0] {
        let bound = saturation(most, fewest, average);
        for (occurrences, length) in postings {
            assert!(
                saturation(occurrences, length, average) <= bound,
                "a posting ({occurrences}, {length}) scored above the bound \
                     at average {average}"
            );
        }
    }
}

/// The distinction the whole pruning path rests on, asserted where a reader
/// will look for it.
///
/// An entry written before the extremes existed reports **no bound**, and a
/// caller has to read that as *do not prune this term*. Reporting a bound of
/// zero instead would say the term can contribute nothing, which is exactly
/// the term a pruning walk discards first — so the two answers differ by the
/// whole result set.
#[test]
fn a_term_whose_entry_has_no_extremes_cannot_be_bounded() {
    let mut corpus = corpus(100, 20.0, &[("lock", 4)]);
    assert_eq!(bound(&corpus, "lock"), None);
    assert_eq!(bound(&corpus, "never-asked"), None);

    corpus
        .terms
        .insert("lock".to_owned(), TermStatistics::bounded(4, 9, 3));
    let held = bound(&corpus, "lock").expect("an entry with extremes bounds");
    assert!(held > 0.0);
}

/// The bound is an upper bound on the whole contribution and not only on its
/// saturation half, so the weight has to be in it.
#[test]
fn a_bound_sits_above_every_score_its_own_postings_can_reach() {
    let mut corpus = corpus(500, 30.0, &[("quorum", 6)]);
    corpus
        .terms
        .insert("quorum".to_owned(), TermStatistics::bounded(6, 11, 4));
    corpus.asked = vec!["quorum".to_owned()];
    let held = bound(&corpus, "quorum").expect("an entry with extremes bounds");

    for (occurrences, length) in [(11, 4), (11, 900), (1, 4), (2, 37), (7, 12)] {
        let posting = posted(&corpus, &[("quorum", occurrences)], length, "quorum");
        assert!(
            posting <= held,
            "a posting ({occurrences}, {length}) scored {posting} above the bound {held}"
        );
    }
}
