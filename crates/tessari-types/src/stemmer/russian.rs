//! The Snowball Russian stemmer, written from the published algorithm
//! (snowballstem.org/algorithms/russian), checked against its vocabulary.
//!
//! Every rule reads only the region after the word's first vowel (RV), so the
//! letters before it are never examined.

use super::word::Word;

const VOWELS: &str = "аеиоуыэюя";

const PERFECTIVE_GERUND_AFTER_A: [&str; 3] = ["в", "вши", "вшись"];
const PERFECTIVE_GERUND: [&str; 6] = ["ив", "ивши", "ившись", "ыв", "ывши", "ывшись"];
const ADJECTIVE: [&str; 26] = [
    "ее", "ие", "ые", "ое", "ими", "ыми", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "его",
    "ого", "ему", "ому", "их", "ых", "ую", "юю", "ая", "яя", "ою", "ею",
];
const PARTICIPLE_AFTER_A: [&str; 5] = ["ем", "нн", "вш", "ющ", "щ"];
const PARTICIPLE: [&str; 3] = ["ивш", "ывш", "ующ"];
const REFLEXIVE: [&str; 2] = ["ся", "сь"];
const VERB_AFTER_A: [&str; 17] = [
    "ла", "на", "ете", "йте", "ли", "й", "л", "ем", "н", "ло", "но", "ет", "ют", "ны", "ть", "ешь",
    "нно",
];
const VERB: [&str; 29] = [
    "ила", "ыла", "ена", "ейте", "уйте", "ите", "или", "ыли", "ей", "уй", "ил", "ыл", "им", "ым",
    "ен", "ило", "ыло", "ено", "ят", "ует", "уют", "ит", "ыт", "ены", "ить", "ыть", "ишь", "ую",
    "ю",
];
const NOUN: [&str; 36] = [
    "а", "ев", "ов", "ие", "ье", "е", "иями", "ями", "ами", "еи", "ии", "и", "ией", "ей", "ой",
    "ий", "й", "иям", "ям", "ием", "ем", "ам", "ом", "о", "у", "ах", "иях", "ях", "ы", "ь", "ию",
    "ью", "ю", "ия", "ья", "я",
];
const DERIVATIONAL: [&str; 2] = ["ост", "ость"];
const SUPERLATIVE: [&str; 2] = ["ейш", "ейше"];

fn vowel(letter: char) -> bool {
    VOWELS.contains(letter)
}

/// Whether `letter` is one this stemmer reads: a lower-case Russian letter.
pub(super) fn reads(letter: char) -> bool {
    matches!(letter, 'а'..='я' | 'ё')
}

/// The stem of one lower-case Russian word.
pub(super) fn stem(text: &str) -> String {
    let mut word = Word::of(&text.replace('ё', "е"));
    let rv = word
        .letters()
        .iter()
        .position(|letter| vowel(*letter))
        .map_or(word.len(), |first| first.saturating_add(1));
    let (_, r2) = word.regions(vowel);

    if !remove_with(
        &mut word,
        rv,
        &PERFECTIVE_GERUND_AFTER_A,
        &PERFECTIVE_GERUND,
    ) {
        remove(&mut word, rv, &REFLEXIVE);
        if remove(&mut word, rv, &ADJECTIVE) {
            remove_with(&mut word, rv, &PARTICIPLE_AFTER_A, &PARTICIPLE);
        } else if !remove_with(&mut word, rv, &VERB_AFTER_A, &VERB) {
            remove(&mut word, rv, &NOUN);
        }
    }
    remove(&mut word, rv, &["и"]);
    if let Some(suffix) = word.longest_from(&DERIVATIONAL, rv)
        && word.start_of(suffix) >= r2
    {
        word.cut(suffix.chars().count());
    }
    tidy(&mut word, rv);
    word.text()
}

/// Remove the longest of `suffixes` lying in RV.
fn remove(word: &mut Word, rv: usize, suffixes: &[&str]) -> bool {
    match word.longest_from(suffixes, rv) {
        Some(suffix) => {
            word.cut(suffix.chars().count());
            true
        }
        _ => false,
    }
}

/// Remove the longest suffix of either group lying in RV, one of the first
/// group only when it follows `а` or `я` itself in RV.
fn remove_with(word: &mut Word, rv: usize, after_a: &[&str], free: &[&str]) -> bool {
    let both: Vec<&str> = after_a.iter().chain(free).copied().collect();
    let Some(suffix) = word.longest_from(&both, rv) else {
        return false;
    };
    let start = word.start_of(suffix);
    if after_a.contains(&suffix) {
        let before = start
            .checked_sub(1)
            .and_then(|at| word.at(at).map(|letter| (at, letter)));
        if !before.is_some_and(|(at, letter)| at >= rv && matches!(letter, 'а' | 'я')) {
            return false;
        }
    }
    word.cut(suffix.chars().count());
    true
}

/// Step 4: undouble `н`, or remove a superlative and undouble, or remove `ь`.
fn tidy(word: &mut Word, rv: usize) {
    let Some(suffix) = word.longest_from(&["ейше", "ейш", "н", "ь"], rv) else {
        return;
    };
    match suffix {
        "н" => {
            if word.ends_with("нн") && word.start_of("нн") >= rv {
                word.cut(1);
            }
        }
        "ь" => word.cut(1),
        _ => {
            debug_assert!(SUPERLATIVE.contains(&suffix));
            word.cut(suffix.chars().count());
            if word.ends_with("нн") && word.start_of("нн") >= rv {
                word.cut(1);
            }
        }
    }
}
