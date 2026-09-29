//! Steps 2 to 5 of the stemmer: the longer suffixes, each only inside its region.

use super::{ends_in_short_syllable, ends_with, is_li_ending, replace, suffix_in_region};

/// Derivational endings, first pass. Longest match wins, and it must lie in R1.
pub(crate) fn step_2(letters: &mut Vec<char>, r1: usize) {
    const PAIRS: [(&str, &str); 25] = [
        ("ization", "ize"),
        ("ational", "ate"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("iveness", "ive"),
        ("tional", "tion"),
        ("biliti", "ble"),
        ("lessli", "less"),
        ("entli", "ent"),
        ("ation", "ate"),
        ("alism", "al"),
        ("aliti", "al"),
        ("ousli", "ous"),
        ("iviti", "ive"),
        ("fulli", "ful"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("abli", "able"),
        ("izer", "ize"),
        ("ator", "ate"),
        ("alli", "al"),
        ("bli", "ble"),
        ("ogi", "og"),
        ("li", ""),
        ("", ""),
    ];
    for (suffix, replacement) in PAIRS {
        if suffix.is_empty() || !ends_with(letters, suffix) {
            continue;
        }
        let length = suffix.len();
        if !suffix_in_region(letters, r1, length) {
            return;
        }
        let before = letters.len().saturating_sub(length);
        match suffix {
            // `ogi` only becomes `og` after an `l`, so `theologi` shortens and
            // nothing else pretends to.
            "ogi" => {
                if before > 0 && letters[before.saturating_sub(1)] == 'l' {
                    replace(letters, length, replacement);
                }
            }
            // A bare `li` comes off only after the letters it attaches to.
            "li" => {
                if before > 0 && is_li_ending(letters[before.saturating_sub(1)]) {
                    letters.truncate(before);
                }
            }
            _ => replace(letters, length, replacement),
        }
        return;
    }
}

/// Derivational endings, second pass.
pub(crate) fn step_3(letters: &mut Vec<char>, r1: usize, r2: usize) {
    const PAIRS: [(&str, &str); 7] = [
        ("ational", "ate"),
        ("tional", "tion"),
        ("alize", "al"),
        ("icate", "ic"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ness", ""),
    ];
    for (suffix, replacement) in PAIRS {
        if !ends_with(letters, suffix) {
            continue;
        }
        let length = suffix.len();
        if suffix_in_region(letters, r1, length) {
            replace(letters, length, replacement);
        }
        return;
    }
    if ends_with(letters, "ful") {
        if suffix_in_region(letters, r1, 3) {
            letters.truncate(letters.len().saturating_sub(3));
        }
        return;
    }
    // `ative` needs R2 rather than R1: it is a long ending and stripping it from
    // a short word leaves a stem that means something else.
    if ends_with(letters, "ative") && suffix_in_region(letters, r2, 5) {
        letters.truncate(letters.len().saturating_sub(5));
    }
}

/// Endings that come off entirely, and only from deep inside the word.
pub(crate) fn step_4(letters: &mut Vec<char>, r2: usize) {
    const SUFFIXES: [&str; 18] = [
        "ement", "ance", "ence", "able", "ible", "ment", "ant", "ent", "ism", "ate", "iti", "ous",
        "ive", "ize", "al", "er", "ic", "ion",
    ];
    for suffix in SUFFIXES {
        if !ends_with(letters, suffix) {
            continue;
        }
        let length = suffix.len();
        if !suffix_in_region(letters, r2, length) {
            return;
        }
        let before = letters.len().saturating_sub(length);
        if suffix == "ion" {
            // `ion` is only an ending after `s` or `t`; elsewhere it is the word.
            if before > 0 && matches!(letters[before.saturating_sub(1)], 's' | 't') {
                letters.truncate(before);
            }
            return;
        }
        letters.truncate(before);
        return;
    }
}

/// The two letters that come off last.
pub(crate) fn step_5(letters: &mut Vec<char>, r1: usize, r2: usize) {
    if ends_with(letters, "e") {
        let before = letters.len().saturating_sub(1);
        // A final `e` goes when it is deep in the word, or when it is in R1 and
        // is not the `e` that lengthens a short syllable — which is the whole
        // difference between `hope` and `hop`.
        if suffix_in_region(letters, r2, 1)
            || (suffix_in_region(letters, r1, 1) && !ends_in_short_syllable(&letters[..before]))
        {
            letters.truncate(before);
        }
        return;
    }
    if ends_with(letters, "l") && suffix_in_region(letters, r2, 1) {
        let before = letters.len().saturating_sub(1);
        if before > 0 && letters[before.saturating_sub(1)] == 'l' {
            letters.truncate(before);
        }
    }
}
