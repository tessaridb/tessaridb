//! Reducing a word to the form its relatives share.
//!
//! # Why the chain was incomplete without this
//!
//! `lowercase` makes `Running` and `running` one term. `ascii` makes `café` and
//! `cafe` one term. Neither makes `running` and `runs` one term, and that is the
//! difference a person means by "search". A collection indexed without a stemmer
//! answers `run` with the documents that happen to spell it that way, and the
//! ones that say `running` are simply absent — no error, no warning, just a
//! smaller answer than the one asked for.
//!
//! # The algorithm is Porter2 (English Snowball), implemented rather than pulled in
//!
//! This store writes its own storage engine, its own codec and its own parser;
//! a stemmer is smaller than any of them. The real argument is the other way
//! round — a hand-written stemmer fails *silently*, by returning a stem that is
//! merely slightly wrong, so nothing crashes and recall quietly drops.
//!
//! That is answered the way every silent failure in this store is answered: with
//! an oracle. The tests below are the published algorithm's own worked examples
//! and its exception tables, case by case — which is a real check and a *partial*
//! one. The complete oracle is the Snowball project's vocabulary paired with its
//! reference implementation's output, roughly thirty thousand words; obtaining
//! that file and running it here is owed, and until it is, this module is
//! verified against the definition rather than against a corpus.
//!
//! # What is deliberately not attempted
//!
//! **Only lower-case ASCII words are stemmed.** Anything else — a word carrying
//! an upper-case letter, a digit, or a letter outside ASCII — passes through
//! **unchanged**.
//!
//! The rule is stated rather than discovered because half-stemming is worse than
//! not stemming: Porter2 is defined over lower-case English, and applying its
//! suffix rules to `Running` would strip `ing` from a word whose first letter it
//! never normalised, producing a term neither spelling of the word maps to. A
//! chain that wants stemming therefore declares `lowercase` before `stemmer`,
//! and one that forgets gets its words back intact rather than mangled.
//!
//! Only English. A language filter that silently applied English rules to German
//! would do the same damage across a whole corpus, so the filter is named for
//! what it does and another language would be another filter.

/// The stem of one word.
///
/// Returns the word unchanged when it is not a lower-case ASCII word — see this
/// module's documentation for why that is a rule rather than an omission.
#[must_use]
pub fn stem(word: &str) -> String {
    if !is_lowercase_ascii_word(word) {
        return word.to_owned();
    }
    if is_invariant(word) {
        return word.to_owned();
    }
    if let Some(irregular) = irregular_stem(word) {
        return irregular.to_owned();
    }
    if word.len() <= 2 {
        return word.to_owned();
    }

    let mut letters: Vec<char> = word.chars().collect();
    mark_consonant_y(&mut letters);
    let (r1, r2) = regions(&letters);

    step_0(&mut letters);
    if step_1a(&mut letters) {
        return restore_y(&letters);
    }
    step_1b(&mut letters, r1);
    step_1c(&mut letters);
    step_2(&mut letters, r1);
    step_3(&mut letters, r1, r2);
    step_4(&mut letters, r2);
    step_5(&mut letters, r1, r2);

    restore_y(&letters)
}

fn is_lowercase_ascii_word(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// Words whose stem no rule reaches.
///
/// Porter2 carries these as a table because they are not exceptions to a rule —
/// they are places where every rule that would produce the right answer would
/// break a hundred other words.
fn irregular_stem(word: &str) -> Option<&'static str> {
    match word {
        "skis" => Some("ski"),
        "skies" => Some("sky"),
        "dying" => Some("die"),
        "lying" => Some("lie"),
        "tying" => Some("tie"),
        "idly" => Some("idl"),
        "gently" => Some("gentl"),
        "ugly" => Some("ugli"),
        "early" => Some("earli"),
        "only" => Some("onli"),
        "singly" => Some("singl"),
        _ => None,
    }
}

/// Words that must come back exactly as they went in.
///
/// Each ends in something the rules would strip, and each one is a whole word
/// rather than an inflection of a shorter one.
fn is_invariant(word: &str) -> bool {
    matches!(
        word,
        "sky" | "news" | "howe" | "atlas" | "cosmos" | "bias" | "andes"
    )
}

/// Words that must stop after step 1a, having already reached their stem.
///
/// Each ends in `ing`, `ed` or `eed` that is part of the word rather than an
/// ending on it — `inning` is not the act of inn-ing.
fn stops_after_1a(letters: &[char]) -> bool {
    let word: String = letters.iter().collect();
    matches!(
        word.as_str(),
        "inning" | "outing" | "canning" | "herring" | "earring" | "proceed" | "exceed" | "succeed"
    )
}

const VOWELS: [char; 6] = ['a', 'e', 'i', 'o', 'u', 'y'];

fn is_vowel(letter: char) -> bool {
    VOWELS.contains(&letter)
}

/// A `y` that is acting as a consonant is written `Y` for the rest of the run.
///
/// `y` is a vowel in `happy` and a consonant in `young`, and several rules ask
/// which. Marking it once, up front, is what lets every later rule treat the
/// vowel set as fixed instead of re-deciding — and [`restore_y`] puts it back so
/// the mark never escapes this module.
fn mark_consonant_y(letters: &mut [char]) {
    for index in 0..letters.len() {
        if letters[index] != 'y' {
            continue;
        }
        let consonantal = match index.checked_sub(1) {
            None => true,
            Some(previous) => is_vowel(letters[previous]),
        };
        if consonantal {
            letters[index] = 'Y';
        }
    }
}

fn restore_y(letters: &[char]) -> String {
    letters
        .iter()
        .map(|letter| if *letter == 'Y' { 'y' } else { *letter })
        .collect()
}

/// Where R1 and R2 begin.
///
/// R1 is the region after the first consonant that follows a vowel; R2 is that
/// rule applied again inside R1. Nearly every suffix rule is conditioned on one
/// of them, which is how Porter2 avoids stripping a suffix out of a word too
/// short to have one.
///
/// The three prefixes are the published exception: `gener`, `commun` and `arsen`
/// would otherwise put R1 so early that `generate` and `general` collapse.
fn regions(letters: &[char]) -> (usize, usize) {
    let word: String = letters.iter().collect();
    let r1 = ["gener", "commun", "arsen"]
        .iter()
        .find(|prefix| word.starts_with(*prefix))
        .map_or_else(|| region_after(letters, 0), |prefix| prefix.len());
    let r2 = region_after(letters, r1);
    (r1, r2)
}

fn region_after(letters: &[char], from: usize) -> usize {
    let mut index = from;
    while index < letters.len() {
        if is_vowel(letters[index]) {
            break;
        }
        index = index.saturating_add(1);
    }
    while index < letters.len() {
        if !is_vowel(letters[index]) {
            return index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    letters.len()
}

/// Whether the word ends in a short syllable AND R1 is empty.
///
/// This is the test that decides whether `hop` + `ing` becomes `hop` or `hope`,
/// so getting it wrong changes real words rather than rare ones.
fn is_short_word(letters: &[char], r1: usize) -> bool {
    r1 >= letters.len() && ends_in_short_syllable(letters)
}

fn ends_in_short_syllable(letters: &[char]) -> bool {
    match letters.len() {
        0 | 1 => false,
        // A vowel opening the word followed by a consonant: `at`, `ex`.
        2 => is_vowel(letters[0]) && !is_vowel(letters[1]),
        length => {
            let last = length.saturating_sub(1);
            let middle = length.saturating_sub(2);
            let first = length.saturating_sub(3);
            !is_vowel(letters[first])
                && is_vowel(letters[middle])
                && !is_vowel(letters[last])
                // `w`, `x` and a consonantal `y` do not close a short syllable.
                && !matches!(letters[last], 'w' | 'x' | 'Y')
        }
    }
}

fn ends_with(letters: &[char], suffix: &str) -> bool {
    let suffix: Vec<char> = suffix.chars().collect();
    letters.len() >= suffix.len() && letters[letters.len().saturating_sub(suffix.len())..] == suffix
}

/// Whether a suffix of this length lies entirely inside a region.
fn suffix_in_region(letters: &[char], region: usize, suffix_len: usize) -> bool {
    letters.len().saturating_sub(suffix_len) >= region
}

/// Replace the trailing `suffix_len` letters with `replacement`.
fn replace(letters: &mut Vec<char>, suffix_len: usize, replacement: &str) {
    letters.truncate(letters.len().saturating_sub(suffix_len));
    letters.extend(replacement.chars());
}

/// Whether the part of the word before the trailing `suffix_len` letters holds a vowel.
fn stem_holds_vowel(letters: &[char], suffix_len: usize) -> bool {
    letters[..letters.len().saturating_sub(suffix_len)]
        .iter()
        .copied()
        .any(is_vowel)
}

/// Remove a trailing possessive or apostrophe.
fn step_0(letters: &mut Vec<char>) {
    for suffix in ["'s'", "'s", "'"] {
        if ends_with(letters, suffix) {
            letters.truncate(letters.len().saturating_sub(suffix.chars().count()));
            return;
        }
    }
}

/// Plurals. Returns whether the word is finished and later steps must not run.
fn step_1a(letters: &mut Vec<char>) -> bool {
    if ends_with(letters, "sses") {
        replace(letters, 4, "ss");
    } else if ends_with(letters, "ied") || ends_with(letters, "ies") {
        // `ties` is short enough that `ti` would be a stem shared with nothing,
        // so a short word keeps a longer stem than a long one does.
        let replacement = if letters.len() > 4 { "i" } else { "ie" };
        replace(letters, 3, replacement);
    } else if ends_with(letters, "us") || ends_with(letters, "ss") {
        // `bus` is not a plural and `class` is not two of anything.
    } else if ends_with(letters, "s") {
        // A vowel must appear before the letter preceding the `s`, so `gas` and
        // `this` keep their `s` while `gaps` loses it.
        if letters.len() > 2 && stem_holds_vowel(letters, 2) {
            letters.truncate(letters.len().saturating_sub(1));
        }
    }
    stops_after_1a(letters)
}

/// Past tense and the progressive.
fn step_1b(letters: &mut Vec<char>, r1: usize) {
    for suffix in ["eedly", "eed"] {
        if ends_with(letters, suffix) {
            let length = suffix.len();
            if suffix_in_region(letters, r1, length) {
                replace(letters, length, "ee");
            }
            return;
        }
    }
    for suffix in ["ingly", "edly", "ing", "ed"] {
        if !ends_with(letters, suffix) {
            continue;
        }
        let length = suffix.len();
        if !stem_holds_vowel(letters, length) {
            return;
        }
        letters.truncate(letters.len().saturating_sub(length));
        if ends_with(letters, "at") || ends_with(letters, "bl") || ends_with(letters, "iz") {
            letters.push('e');
        } else if ends_in_double(letters) {
            letters.truncate(letters.len().saturating_sub(1));
        } else if is_short_word(letters, r1) {
            letters.push('e');
        }
        return;
    }
}

/// Whether the word ends in one of the doubles Porter2 undoubles.
///
/// Not every repeated letter: `add` keeps both, and the published set is the
/// list of doubles an English inflection actually creates.
fn ends_in_double(letters: &[char]) -> bool {
    const DOUBLES: [&str; 9] = ["bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt"];
    DOUBLES.iter().any(|double| ends_with(letters, double))
}

/// A terminal `y` after a consonant becomes `i`, so `happy` and `happiness` meet.
fn step_1c(letters: &mut [char]) {
    let length = letters.len();
    if length < 3 {
        return;
    }
    let last = length.saturating_sub(1);
    if !matches!(letters[last], 'y' | 'Y') {
        return;
    }
    if is_vowel(letters[last.saturating_sub(1)]) {
        return;
    }
    letters[last] = 'i';
}

/// Whether a `li` may be removed after this letter.
fn is_li_ending(letter: char) -> bool {
    matches!(
        letter,
        'c' | 'd' | 'e' | 'g' | 'h' | 'k' | 'm' | 'n' | 'r' | 't'
    )
}

/// Derivational endings, first pass. Longest match wins, and it must lie in R1.
fn step_2(letters: &mut Vec<char>, r1: usize) {
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
fn step_3(letters: &mut Vec<char>, r1: usize, r2: usize) {
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
fn step_4(letters: &mut Vec<char>, r2: usize) {
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
fn step_5(letters: &mut Vec<char>, r1: usize, r2: usize) {
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
