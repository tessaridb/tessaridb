//! The language stemmers against the Snowball project's own vocabularies
//! (G051 T7.3).
//!
//! Each `golden/<language>.txt` is every 20th word of the published
//! `voc.txt` beside its line of `output.txt`, from
//! github.com/snowballstem/snowball-data (BSD-3-Clause, copyright Dr Martin
//! Porter and Richard Boulton; the notice is the first lines of each file).
//! The whole vocabulary is checked by the ignored test below, pointed at a
//! directory holding `<language>-voc.txt` and `<language>-output.txt`.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::analyzer::Language;

use super::stem_in;

/// Every pair of `golden` this stemmer gets wrong, as `word -> got (wanted)`.
fn wrong(language: Language, pairs: impl Iterator<Item = (String, String)>) -> Vec<String> {
    pairs
        .filter_map(|(word, wanted)| {
            let got = stem_in(language, &word);
            (got != wanted).then(|| format!("{word} -> {got} ({wanted})"))
        })
        .collect()
}

/// The pairs of a golden file: `word stem` per line, `#` lines a notice.
fn golden(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let (word, stem) = line.split_once(' ').unwrap();
            (word.to_owned(), stem.to_owned())
        })
}

fn check(language: Language, text: &str) {
    let pairs: Vec<(String, String)> = golden(text).collect();
    assert!(pairs.len() > 500, "{language:?}: {} pairs", pairs.len());
    let wrong = wrong(language, pairs.into_iter());
    assert!(
        wrong.is_empty(),
        "{language:?}: {} wrong: {:?}",
        wrong.len(),
        &wrong[..wrong.len().min(30)]
    );
}

#[test]
fn russian_stems_its_published_vocabulary() {
    check(Language::Russian, include_str!("golden/russian.txt"));
}

#[test]
fn german_stems_its_published_vocabulary() {
    check(Language::German, include_str!("golden/german.txt"));
}

#[test]
fn spanish_stems_its_published_vocabulary() {
    check(Language::Spanish, include_str!("golden/spanish.txt"));
}

#[test]
fn french_stems_its_published_vocabulary() {
    check(Language::French, include_str!("golden/french.txt"));
}

/// Every word of every published vocabulary:
/// `TESSARIDB_SNOWBALL_DATA=<dir> cargo test -p tessari-types the_whole -- --ignored`.
#[test]
#[ignore = "reads the whole Snowball vocabularies from a directory outside the repository"]
fn the_whole_published_vocabularies() {
    let directory = std::env::var("TESSARIDB_SNOWBALL_DATA").unwrap();
    for (language, name) in [
        (Language::Russian, "russian"),
        (Language::German, "german"),
        (Language::Spanish, "spanish"),
        (Language::French, "french"),
    ] {
        let read =
            |file: &str| std::fs::read_to_string(format!("{directory}/{name}-{file}")).unwrap();
        let (words, stems) = (read("voc.txt"), read("output.txt"));
        let pairs = words
            .lines()
            .zip(stems.lines())
            .map(|(word, stem)| (word.to_owned(), stem.to_owned()));
        let wrong = wrong(language, pairs);
        assert!(
            wrong.is_empty(),
            "{name}: {} of {} wrong: {:?}",
            wrong.len(),
            words.lines().count(),
            &wrong[..wrong.len().min(40)]
        );
    }
}
