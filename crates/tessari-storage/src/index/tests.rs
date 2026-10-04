#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_types::{Analyzer, DatabaseId, Filter, IndexId, NamespaceId, Path, TableId, Value};

use super::terms_of;
use crate::catalog::IndexDefinition;

fn definition() -> IndexDefinition {
    IndexDefinition {
        id: IndexId::new(1),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        name: "by_body".to_owned(),
        fields: vec![Path::field("body")],
        search: true,
        unique: false,
        quantized: false,
        vector: None,
        spatial: false,
        costs: crate::catalog::SearchCosts::default(),
        engine: None,
        tokenizer: None,
    }
}

fn record(text: &str) -> Value {
    Value::Object(
        [("body".to_owned(), Value::from(text))]
            .into_iter()
            .collect(),
    )
}

/// The frequency of each term, keyed by the term as written.
fn analysed(text: &str) -> (Vec<u32>, u64) {
    let analyzer = Analyzer::new(vec![Filter::Lowercase]);
    let found = terms_of(&definition(), Some(&analyzer), &record(text));
    (
        found.postings.iter().map(|(_, count)| *count).collect(),
        found.tokens,
    )
}

#[test]
fn a_word_twice_is_one_posting_that_says_twice() {
    // The whole of what changed here: the terms are still deduplicated into
    // one posting each, but the run length is no longer thrown away on the
    // way. `dedup()` discarded exactly this number.
    let (frequencies, tokens) = analysed("lock lock contention");
    assert_eq!(frequencies.len(), 2, "two distinct terms");
    assert_eq!(frequencies.iter().sum::<u32>(), 3, "three tokens posted");
    assert!(frequencies.contains(&2), "the repeated term says 2");
    assert_eq!(tokens, 3, "length counts repeats");
}

#[test]
fn every_term_of_a_text_with_no_repeats_says_once() {
    let (frequencies, tokens) = analysed("lock contention here");
    assert_eq!(frequencies, vec![1, 1, 1]);
    assert_eq!(tokens, 3);
}

#[test]
fn the_frequency_is_taken_after_the_filters_and_not_before() {
    // `Lock` and `lock` are one term once lowercased, so they are one posting
    // with a frequency of two. Counting before the filters would report two
    // postings of one, which is the same mistake as scoring the spelling
    // rather than the word.
    let (frequencies, tokens) = analysed("Lock lock");
    assert_eq!(frequencies, vec![2]);
    assert_eq!(tokens, 2);
}

#[test]
fn a_field_with_no_analyzer_posts_nothing_and_has_no_length() {
    let found = terms_of(&definition(), None, &record("lock contention"));
    assert!(found.postings.is_empty());
    assert_eq!(found.tokens, 0);
}

#[test]
fn the_length_a_posting_states_saturates_rather_than_wrapping() {
    // A wrapped length would make one absurd record's score wrong by an
    // arbitrary amount while every other record still looked right.
    let mut found = terms_of(&definition(), None, &record(""));
    found.tokens = u64::from(u32::MAX) + 1;
    assert_eq!(found.length(), u32::MAX);
}
