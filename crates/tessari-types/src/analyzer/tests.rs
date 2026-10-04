#![allow(clippy::panic)]

use super::{Analyzer, Filter, Language};

fn simple() -> Analyzer {
    Analyzer::new(vec![Filter::Lowercase, Filter::Ascii])
}

#[test]
fn text_becomes_the_words_it_holds() {
    assert_eq!(
        simple().terms("Ada Lovelace, 1843!"),
        vec!["ada", "lovelace", "1843"]
    );
}

#[test]
fn punctuation_contributes_no_terms() {
    assert_eq!(simple().terms("  ...  "), Vec::<String>::new());
    assert_eq!(simple().terms(""), Vec::<String>::new());
}

#[test]
fn the_filters_are_what_makes_two_spellings_one_term() {
    assert_eq!(simple().terms("Café"), simple().terms("cafe"));
    assert_eq!(simple().terms("Lovelace"), simple().terms("lovelace"));
}

#[test]
fn an_analyzer_with_no_filters_keeps_the_words_as_written() {
    let bare = Analyzer::default();
    assert_eq!(bare.terms("Ada Lovelace"), vec!["Ada", "Lovelace"]);
}

#[test]
fn a_letter_the_fold_does_not_know_is_still_a_letter() {
    // Dropping it would silently shorten a term and make a search miss.
    let folded = Analyzer::new(vec![Filter::Ascii]);
    // (Each ideograph is its own token — see the tokenizer — and none is
    // lost to the fold.)
    assert_eq!(folded.terms("日本語"), vec!["日", "本", "語"]);
    // `ó` is in the table and folds; `Ł` and `ź` are not, and survive
    // rather than being dropped.
    assert_eq!(folded.terms("Łódź"), vec!["Łodź"]);
}

#[test]
fn every_filter_is_findable_by_its_own_name() {
    for filter in Filter::ALL {
        assert_eq!(Filter::parse(filter.name()), Some(*filter));
        assert_eq!(Filter::parse(&filter.name().to_uppercase()), Some(*filter));
    }
    assert_eq!(Filter::parse("porter"), None);
}

#[test]
fn the_chain_that_makes_two_words_rather_than_two_spellings_meet() {
    // The order is the caller's and it matters: the stemmer is defined over
    // lower-case words, so `lowercase` has to come first or `Running` is
    // returned untouched rather than half-stemmed.
    let full = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    assert_eq!(full.terms("Running quickly"), vec!["run", "quick"]);
    assert_eq!(full.terms("He runs"), vec!["he", "run"]);

    // Without the stemmer these are two different terms, which is the whole
    // reason the filter exists.
    assert_ne!(simple().terms("running"), simple().terms("runs"));
    assert_eq!(full.terms("running"), full.terms("runs"));
}

/// Whether any stored term begins with any spelling of any typed word.
///
/// The matching rule `MATCHES PREFIX` applies, written here so the tests
/// assert the rule rather than a re-derivation of it.
fn reaches(analyzer: &Analyzer, stored: &str, typed: &str) -> bool {
    let terms = analyzer.terms(stored);
    let asked = analyzer.prefixes(typed);
    !asked.is_empty()
        && asked.iter().all(|alternatives| {
            alternatives
                .iter()
                .any(|prefix| terms.iter().any(|term| term.starts_with(prefix)))
        })
}

#[test]
fn a_prefix_is_folded_but_never_stemmed() {
    let full = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    // The folding applies: a prefix of a lower-cased, unaccented word is
    // what the dictionary holds.
    assert_eq!(full.prefixes("VECto"), vec![vec!["vecto".to_owned()]]);
    assert_eq!(full.prefixes("Café"), vec![vec!["cafe".to_owned()]]);
    // The stemming does not remove the typed spelling — it adds one beside
    // it. `runni` stems to itself, so there is only the one.
    assert_eq!(full.prefixes("runni"), vec![vec!["runni".to_owned()]]);
    assert_eq!(full.terms("running"), vec!["run"]);
    // A prefix longer than the stem and not a word of its own reaches
    // nothing, because the store never held those letters.
    assert!(!reaches(&full, "running", "runni"));
    // A prefix at or below the stem does reach it.
    assert!(reaches(&full, "running", "ru"));
}

#[test]
fn a_complete_word_is_a_prefix_of_itself_even_when_it_stems_to_something_shorter() {
    // The case that makes the second spelling necessary, and the one a
    // reader hits first: typing all of a word rather than most of it.
    let full = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    assert_eq!(full.terms("contention"), vec!["content"]);
    assert!(reaches(&full, "Locking and contention", "conten"));
    assert!(reaches(&full, "Locking and contention", "contention"));
    assert!(reaches(&full, "Running a compaction", "running"));
    // Which is the property in general: a prefix reaches at least what an
    // exact term match reaches.
    for word in ["contention", "running", "locking", "compaction"] {
        let stored = "Locking and contention while running a compaction";
        let terms = full.terms(stored);
        let exact = full.terms(word);
        assert!(exact.iter().all(|term| terms.contains(term)), "{word}");
        assert!(reaches(&full, stored, word), "{word}");
    }
}

#[test]
fn a_chain_with_no_stemmer_offers_one_spelling_per_word() {
    // The two alternatives coincide, so on a field that does not stem there
    // is no asymmetry and no second walk to pay for.
    for text in ["Ada Lovelace", "Café au lait", "1843"] {
        let asked = simple().prefixes(text);
        let terms = simple().terms(text);
        assert_eq!(asked.len(), terms.len(), "{text}");
        for (alternatives, term) in asked.iter().zip(&terms) {
            assert_eq!(alternatives, &vec![term.clone()], "{text}");
        }
    }
}

#[test]
fn the_spans_and_the_terms_are_two_readings_of_one_walk() {
    // Asserted directly rather than inferred from a passing highlight,
    // because this is the property the whole offset source rests on: a
    // second tokenizer that agreed today would drift silently.
    let full = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    for text in [
        "Ada Lovelace, 1843!",
        "  ...  ",
        "",
        "Running quickly",
        "Café au lait",
        "日本語 and Łódź",
        "trailing",
        "1843",
    ] {
        let walked: Vec<String> = full.spans(text).into_iter().map(|t| t.term).collect();
        assert_eq!(walked, full.terms(text), "{text}");
    }
}

#[test]
fn a_span_covers_the_bytes_the_token_occupied_and_not_the_term_it_became() {
    // The criterion's deciding case in miniature: the reader typed three
    // letters, the text holds seven, and the highlight is over the seven.
    let full = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    let text = "He was Running fast";
    let spans = full.spans(text);
    let running = spans
        .iter()
        .find(|token| token.term == "run")
        .expect("the text holds a word that stems to run");
    assert_eq!(running.bytes, 7..14);
    assert_eq!(&text[running.bytes.clone()], "Running");
}

#[test]
fn a_multibyte_character_contributes_its_real_byte_width() {
    // `é` is two bytes, so a span counted in characters would be short by
    // one and the mark would stop mid-letter.
    let folded = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii]);
    let text = "un Café ici";
    let spans = folded.spans(text);
    let cafe = spans
        .iter()
        .find(|token| token.term == "cafe")
        .expect("the text holds a folded café");
    assert_eq!(cafe.bytes, 3..8);
    assert_eq!(&text[cafe.bytes.clone()], "Café");
    // Every range slices, which is the invariant `char_indices` buys.
    for token in &spans {
        assert!(text.get(token.bytes.clone()).is_some(), "{token:?}");
    }
}

#[test]
fn punctuation_opens_no_token_and_so_leaves_no_span_to_mark() {
    // Nothing became a term, so there is nothing a highlight could claim
    // matched — and the spans agree with the terms about that.
    assert!(simple().spans("  ...  ").is_empty());
    assert!(simple().spans("").is_empty());
}

#[test]
fn a_stemmer_without_lowercase_in_front_of_it_leaves_the_word_alone() {
    // Stated as a test rather than a comment: half-stemming would produce a
    // term neither spelling of the word reaches, so the filter declines.
    let bare = Analyzer::new(vec![Filter::Stemmer(Language::English)]);
    assert_eq!(bare.terms("Running"), vec!["Running"]);
    assert_eq!(bare.terms("running"), vec!["run"]);
}

#[test]
fn an_ideograph_is_a_token_of_its_own_and_its_bytes_still_slice() {
    // A sentence of Han characters carries no spaces, so splitting on
    // non-letters made one term of it and nothing inside could be found.
    let text = "東京都に行く Tokyo";
    assert_eq!(
        simple().terms(text),
        vec!["東", "京", "都", "に", "行", "く", "tokyo"]
    );
    for token in simple().spans(text) {
        assert!(text.get(token.bytes.clone()).is_some(), "{token:?}");
    }
    // Katakana and Hangul keep their runs: Katakana words are written
    // together and Hangul separates its words with spaces.
    assert_eq!(
        simple().terms("コンピュータ 서울 시"),
        vec!["コンピュータ", "서울", "시"]
    );
    // The two halves of a mixed run each keep their own rule.
    assert_eq!(simple().terms("ab東c"), vec!["ab", "東", "c"]);
}

#[test]
fn surfaces_are_the_chain_without_its_stemmers_one_for_one() {
    let english = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    let text = "Transactions were Running";
    assert_eq!(english.terms(text), vec!["transact", "were", "run"]);
    assert_eq!(
        english.surfaces(text),
        vec!["transactions", "were", "running"]
    );
    // With no stemmer the surface is the term.
    assert_eq!(simple().surfaces(text), simple().terms(text));
}

#[test]
fn a_remembered_analysis_is_the_same_analysis() {
    let english = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    let mut memo = super::Memo::default();
    for text in [
        "Transactions were Running, running and RUNNING",
        "東京都 Café — running again",
        "",
        "  ...  ",
    ] {
        // Twice, so the second pass is answered from the memo.
        for _ in 0..2 {
            assert_eq!(
                english.analysed(text, &mut memo),
                (english.terms(text), english.surfaces(text)),
                "{text}"
            );
        }
    }
}
