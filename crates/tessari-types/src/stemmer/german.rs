//! The Snowball German stemmer, written from the published algorithm
//! (snowballstem.org/algorithms/german), checked against its vocabulary.

use super::word::Word;

fn vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö' | 'ü')
}

/// Whether `letter` is one this stemmer reads.
pub(super) fn reads(letter: char) -> bool {
    letter.is_ascii_lowercase() || matches!(letter, 'ä' | 'ö' | 'ü' | 'ß' | '\'')
}

fn s_ending(letter: char) -> bool {
    matches!(
        letter,
        'b' | 'd' | 'f' | 'g' | 'h' | 'k' | 'l' | 'm' | 'n' | 'r' | 't'
    )
}

fn st_ending(letter: char) -> bool {
    letter != 'r' && s_ending(letter)
}

fn et_ending(letter: char) -> bool {
    matches!(
        letter,
        'd' | 'f' | 'g' | 'k' | 'l' | 'm' | 'n' | 'r' | 's' | 't' | 'U' | 'z' | 'ä'
    )
}

/// The stem of one lower-case German word.
pub(super) fn stem(text: &str) -> String {
    let mut word = prelude(text);
    let (r1, r2) = word.regions(vowel);
    let r1 = r1.max(3);
    step_1(&mut word, r1);
    step_2(&mut word, r1);
    step_3(&mut word, r1, r2);
    apostrophe(&mut word);
    word.letters()
        .iter()
        .map(|letter| match letter {
            'U' | 'ü' => 'u',
            'Y' => 'y',
            'ä' => 'a',
            'ö' => 'o',
            other => *other,
        })
        .collect()
}

/// `u` and `y` between vowels marked, then `ß`, `ae`, `oe` and `ue` (not after
/// `q`) written as the letters they stand for.
fn prelude(text: &str) -> Word {
    let mut letters: Vec<char> = text.chars().collect();
    for at in 1..letters.len().saturating_sub(1) {
        let around = letters
            .get(at.saturating_sub(1))
            .copied()
            .is_some_and(vowel)
            && letters
                .get(at.saturating_add(1))
                .copied()
                .is_some_and(vowel);
        if let Some(letter) = letters.get_mut(at)
            && around
        {
            match *letter {
                'u' => *letter = 'U',
                'y' => *letter = 'Y',
                _ => {}
            }
        }
    }
    let mut mapped = Vec::with_capacity(letters.len());
    let mut at = 0;
    while let Some(letter) = letters.get(at).copied() {
        let next = letters.get(at.saturating_add(1)).copied();
        let joined = match (letter, next) {
            ('a', Some('e')) => Some('ä'),
            ('o', Some('e')) => Some('ö'),
            ('u', Some('e')) if mapped.last() != Some(&'q') => Some('ü'),
            _ => None,
        };
        if let Some(joined) = joined {
            mapped.push(joined);
            at = at.saturating_add(2);
        } else if letter == 'ß' {
            mapped.extend(['s', 's']);
            at = at.saturating_add(1);
        } else {
            mapped.push(letter);
            at = at.saturating_add(1);
        }
    }
    Word::of(&mapped.into_iter().collect::<String>())
}

fn step_1(word: &mut Word, r1: usize) {
    let Some(suffix) = word.longest(&[
        "em", "ern", "er", "e", "en", "es", "s", "erin", "erinnen", "ln", "lns",
    ]) else {
        return;
    };
    let start = word.start_of(suffix);
    if start < r1 {
        return;
    }
    let count = suffix.chars().count();
    match suffix {
        "em" => {
            if !word.before_is(start, "syst") {
                word.cut(count);
            }
        }
        "s" => {
            if start
                .checked_sub(1)
                .and_then(|at| word.at(at))
                .is_some_and(s_ending)
            {
                word.cut(count);
            }
        }
        "ln" | "lns" => word.replace(count, "l"),
        "e" | "en" | "es" => {
            word.cut(count);
            if word.ends_with("niss") {
                word.cut(1);
            }
        }
        _ => word.cut(count),
    }
}

fn step_2(word: &mut Word, r1: usize) {
    let Some(suffix) = word.longest(&["en", "er", "est", "st", "et"]) else {
        return;
    };
    let start = word.start_of(suffix);
    if start < r1 {
        return;
    }
    let before = start.checked_sub(1).and_then(|at| word.at(at));
    let keep = match suffix {
        "st" => !(before.is_some_and(st_ending) && start >= 4),
        "et" => {
            !before.is_some_and(et_ending)
                || ["geordn", "intern", "plan", "tick", "tr"]
                    .iter()
                    .any(|stem| word.before_is(start, stem))
        }
        _ => false,
    };
    if !keep {
        word.cut(suffix.chars().count());
    }
}

fn step_3(word: &mut Word, r1: usize, r2: usize) {
    let Some(suffix) = word.longest(&["end", "ung", "ig", "ik", "isch", "lich", "heit", "keit"])
    else {
        return;
    };
    let start = word.start_of(suffix);
    if start < r2 {
        return;
    }
    let count = suffix.chars().count();
    match suffix {
        "end" | "ung" => {
            word.cut(count);
            if word.ends_with("ig")
                && word.start_of("ig") >= r2
                && !word.before_is(word.start_of("ig"), "e")
            {
                word.cut(2);
            }
        }
        "ig" | "ik" | "isch" => {
            if !word.before_is(start, "e") {
                word.cut(count);
            }
        }
        "lich" | "heit" => {
            word.cut(count);
            if let Some(more) = word.longest(&["er", "en"])
                && word.start_of(more) >= r1
            {
                word.cut(2);
            }
        }
        _ => {
            word.cut(count);
            if let Some(more) = word.longest(&["lich", "ig"])
                && word.start_of(more) >= r2
            {
                word.cut(more.chars().count());
            }
        }
    }
}

/// `'s`, `'sch` or `'` removed when at least two letters remain.
fn apostrophe(word: &mut Word) {
    if let Some(suffix) = word.longest(&["'s", "'sch", "'"])
        && word.start_of(suffix) >= 2
    {
        word.cut(suffix.chars().count());
    }
}
