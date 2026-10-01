//! The Snowball French stemmer, written from the published algorithm
//! (snowballstem.org/algorithms/french), checked against its vocabulary.
//!
//! Marked letters (`U`, `I`, `Y`, and `H` before a vowel that carried a
//! diaeresis) are not vowels while the rules run, and are turned back after.

use super::word::Word;

fn vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e'
            | 'i'
            | 'o'
            | 'u'
            | 'y'
            | 'â'
            | 'à'
            | 'ë'
            | 'é'
            | 'ê'
            | 'è'
            | 'ï'
            | 'î'
            | 'ô'
            | 'û'
            | 'ù'
    )
}

/// Whether `letter` is one this stemmer reads.
pub(super) fn reads(letter: char) -> bool {
    letter.is_ascii_lowercase()
        || matches!(
            letter,
            'â' | 'à'
                | 'ç'
                | 'ë'
                | 'é'
                | 'ê'
                | 'è'
                | 'ï'
                | 'î'
                | 'ô'
                | 'û'
                | 'ù'
                | 'ÿ'
                | 'œ'
                | 'æ'
                | '\''
        )
}

const I_VERB: [&str; 35] = [
    "îmes", "ît", "îtes", "i", "ie", "ies", "ir", "ira", "irai", "iraIent", "irais", "irait",
    "iras", "irent", "irez", "iriez", "irions", "irons", "iront", "is", "issaIent", "issais",
    "issait", "issant", "issante", "issantes", "issants", "isse", "issent", "isses", "issez",
    "issiez", "issions", "issons", "it",
];

const E_VERB: [&str; 19] = [
    "é", "ée", "ées", "és", "èrent", "er", "era", "erai", "eraIent", "erais", "erait", "eras",
    "erez", "eriez", "erions", "erons", "eront", "ez", "iez",
];

const A_VERB: [&str; 17] = [
    "âmes", "ât", "âtes", "a", "ai", "aIent", "ait", "ant", "ante", "antes", "ants", "as", "asse",
    "assent", "asses", "assiez", "assions",
];

const STANDARD: [&str; 44] = [
    "ance",
    "iqUe",
    "isme",
    "able",
    "iste",
    "eux",
    "ances",
    "iqUes",
    "ismes",
    "ables",
    "istes",
    "atrice",
    "ateur",
    "ation",
    "atrices",
    "ateurs",
    "ations",
    "logie",
    "logies",
    "usion",
    "ution",
    "usions",
    "utions",
    "ence",
    "ences",
    "ement",
    "ements",
    "ité",
    "ités",
    "if",
    "ive",
    "ifs",
    "ives",
    "eaux",
    "aux",
    "oux",
    "euse",
    "euses",
    "issement",
    "issements",
    "amment",
    "emment",
    "ment",
    "ments",
];

/// Where the regions begin.
struct Regions {
    rv: usize,
    r1: usize,
    r2: usize,
}

/// The stem of one lower-case French word.
pub(super) fn stem(text: &str) -> String {
    let mut word = prelude(elided(text));
    let regions = Regions {
        rv: rv_of(&word),
        r1: word.regions(vowel).0,
        r2: word.regions(vowel).1,
    };
    let before = word.text();
    let (removed, then_verbs) = standard(&mut word, &regions);
    // Step 1 ends the suffix steps unless it found an `-ment` ending; then
    // 2a, then 2b, and the residual step only when none of them altered it.
    let altered = (removed && !then_verbs)
        || i_verb(&mut word, regions.rv)
        || e_or_a_verb(&mut word, &regions);
    if !altered {
        residual(&mut word, &regions);
    }
    if altered && word.text() != before {
        if word.ends_with("Y") {
            word.replace(1, "i");
        } else if word.ends_with("ç") {
            word.replace(1, "c");
        }
    }
    if word
        .longest(&["enn", "onn", "ett", "ell", "eill"])
        .is_some()
    {
        word.cut(1);
    }
    unaccent(&mut word);
    postlude(&word)
}

/// The word with an elided article or pronoun (`l'`, `qu'`, …) removed.
fn elided(text: &str) -> &str {
    let Some((head, rest)) = text.split_once('\'') else {
        return text;
    };
    let elides = head == "qu"
        || (head.chars().count() == 1
            && head.chars().all(|letter| {
                matches!(letter, 'c' | 'd' | 'j' | 'l' | 'm' | 'n' | 's' | 't' | 'z')
            }));
    if elides && !rest.is_empty() {
        rest
    } else {
        text
    }
}

/// `u` and `i` between vowels, `y` beside one and `u` after `q` marked; `ë`
/// and `ï` written as `He` and `Hi`.
fn prelude(text: &str) -> Word {
    let mut letters: Vec<char> = Vec::with_capacity(text.len());
    for letter in text.chars() {
        match letter {
            'ë' => letters.extend(['H', 'e']),
            'ï' => letters.extend(['H', 'i']),
            other => letters.push(other),
        }
    }
    for at in 0..letters.len() {
        let before = at.checked_sub(1).and_then(|at| letters.get(at)).copied();
        let after = letters.get(at.saturating_add(1)).copied();
        let (vowel_before, vowel_after) = (before.is_some_and(vowel), after.is_some_and(vowel));
        let Some(letter) = letters.get_mut(at) else {
            continue;
        };
        *letter = match *letter {
            'u' | 'i' if vowel_before && vowel_after => letter.to_ascii_uppercase(),
            'y' if vowel_before || vowel_after => 'Y',
            'u' if before == Some('q') => 'U',
            other => other,
        };
    }
    Word::of(&letters.into_iter().collect::<String>())
}

fn rv_of(word: &Word) -> usize {
    let end = word.len();
    let starts = |prefix: &str| word.letters().iter().copied().take(3).eq(prefix.chars());
    let first_two_vowels =
        word.at(0).is_some_and(vowel) && word.at(1).is_some_and(vowel) && end > 2;
    let ni_vowel =
        word.at(0) == Some('n') && word.at(1) == Some('i') && word.at(2).is_some_and(vowel);
    if first_two_vowels || starts("par") || starts("col") || starts("tap") || ni_vowel {
        return 3.min(end);
    }
    (1..end)
        .find(|at| word.at(*at).is_some_and(vowel))
        .map_or(end, |at| at.saturating_add(1))
}

/// Step 1: whether a suffix was removed, and whether the verb steps still run
/// after it (`amment`, `emment`, `ment`, `ments`).
fn standard(word: &mut Word, regions: &Regions) -> (bool, bool) {
    let Regions { rv, r1, r2 } = *regions;
    let Some(suffix) = word.longest(&STANDARD) else {
        return (false, false);
    };
    let start = word.start_of(suffix);
    let count = suffix.chars().count();
    let before = start.checked_sub(1).and_then(|at| word.at(at));
    let removed = match suffix {
        "ance" | "iqUe" | "isme" | "able" | "iste" | "eux" | "ances" | "iqUes" | "ismes"
        | "ables" | "istes" => cut_in(word, start >= r2, count),
        "atrice" | "ateur" | "ation" | "atrices" | "ateurs" | "ations" => {
            cut_in(word, start >= r2, count) && {
                if word.ends_with("ic") {
                    if word.start_of("ic") >= r2 {
                        word.cut(2);
                    } else {
                        word.replace(2, "iqU");
                    }
                }
                true
            }
        }
        "logie" | "logies" => replace_in(word, start >= r2, count, "log"),
        "usion" | "ution" | "usions" | "utions" => replace_in(word, start >= r2, count, "u"),
        "ence" | "ences" => replace_in(word, start >= r2, count, "ent"),
        "ement" | "ements" => {
            cut_in(word, start >= rv, count) && {
                ement(word, regions);
                true
            }
        }
        "ité" | "ités" => {
            cut_in(word, start >= r2, count) && {
                if let Some(more) = word.longest(&["abil", "ic", "iv"]) {
                    let at = word.start_of(more);
                    match more {
                        "abil" if at >= r2 => word.cut(4),
                        "abil" => word.replace(4, "abl"),
                        "ic" if at >= r2 => word.cut(2),
                        "ic" => word.replace(2, "iqU"),
                        _ if at >= r2 => word.cut(2),
                        _ => {}
                    }
                }
                true
            }
        }
        "if" | "ive" | "ifs" | "ives" => {
            cut_in(word, start >= r2, count) && {
                if word.ends_with("at") && word.start_of("at") >= r2 {
                    word.cut(2);
                    if word.ends_with("ic") {
                        if word.start_of("ic") >= r2 {
                            word.cut(2);
                        } else {
                            word.replace(2, "iqU");
                        }
                    }
                }
                true
            }
        }
        "eaux" => replace_in(word, true, count, "eau"),
        "aux" => replace_in(word, start >= r1, count, "al"),
        "oux" => replace_in(
            word,
            before.is_some_and(|letter| matches!(letter, 'b' | 'h' | 'j' | 'l' | 'n' | 'p')),
            count,
            "ou",
        ),
        "euse" | "euses" => {
            if start >= r2 {
                word.cut(count);
                true
            } else {
                replace_in(word, start >= r1, count, "eux")
            }
        }
        "issement" | "issements" => cut_in(
            word,
            start >= r1 && before.is_some_and(|letter| !vowel(letter)),
            count,
        ),
        "amment" => return (replace_in(word, start >= rv, count, "ant"), true),
        "emment" => return (replace_in(word, start >= rv, count, "ent"), true),
        _ => {
            let vowel_in_rv = start
                .checked_sub(1)
                .is_some_and(|at| at >= rv && word.at(at).is_some_and(vowel));
            return (cut_in(word, vowel_in_rv, count), true);
        }
    };
    (removed, false)
}

/// What precedes a removed `ement`.
fn ement(word: &mut Word, regions: &Regions) {
    let Regions { rv, r1, r2 } = *regions;
    let Some(more) = word.longest(&["iv", "eus", "abl", "iqU", "ièr", "Ièr"]) else {
        return;
    };
    let at = word.start_of(more);
    match more {
        "iv" => {
            if at >= r2 {
                word.cut(2);
                if word.ends_with("at") && word.start_of("at") >= r2 {
                    word.cut(2);
                }
            }
        }
        "eus" => {
            if at >= r2 {
                word.cut(3);
            } else if at >= r1 {
                word.replace(3, "eux");
            }
        }
        "abl" | "iqU" => {
            if at >= r2 {
                word.cut(3);
            }
        }
        _ => {
            if at >= rv {
                word.replace(3, "i");
            }
        }
    }
}

fn cut_in(word: &mut Word, inside: bool, count: usize) -> bool {
    if inside {
        word.cut(count);
    }
    inside
}

fn replace_in(word: &mut Word, inside: bool, count: usize, with: &str) -> bool {
    if inside {
        word.replace(count, with);
    }
    inside
}

/// Step 2a.
fn i_verb(word: &mut Word, rv: usize) -> bool {
    let Some(suffix) = word.longest_from(&I_VERB, rv) else {
        return false;
    };
    let Some(at) = word.start_of(suffix).checked_sub(1) else {
        return false;
    };
    let fits = at >= rv
        && word
            .at(at)
            .is_some_and(|letter| !vowel(letter) && letter != 'H');
    cut_in(word, fits, suffix.chars().count())
}

/// Step 2b.
fn e_or_a_verb(word: &mut Word, regions: &Regions) -> bool {
    let Regions { rv, r2, .. } = *regions;
    let mut all: Vec<&str> = vec!["ions", "ais", "aise", "aises"];
    all.extend(E_VERB);
    all.extend(A_VERB);
    let Some(suffix) = word.longest_from(&all, rv) else {
        return false;
    };
    let start = word.start_of(suffix);
    let count = suffix.chars().count();
    match suffix {
        "ions" => cut_in(word, start >= r2, count),
        "ais" | "aise" | "aises" => {
            let kept = (start == 3 && word.before_is(start, "al"))
                || word.before_is(start, "auv")
                || word.before_is(start, "épl");
            // Removed like the other endings in `a`, the `e` before it with it.
            cut_in(word, !kept, count) && {
                if word.ends_with("e") && word.start_of("e") >= rv {
                    word.cut(1);
                }
                true
            }
        }
        _ if A_VERB.contains(&suffix) => {
            word.cut(count);
            if word.ends_with("e") && word.start_of("e") >= rv {
                word.cut(1);
            }
            true
        }
        _ => {
            word.cut(count);
            true
        }
    }
}

/// Step 4.
fn residual(word: &mut Word, regions: &Regions) {
    let Regions { rv, r2, .. } = *regions;
    if word.ends_with("s") {
        let at = word.start_of("s");
        let before = at.checked_sub(1).and_then(|at| word.at(at));
        let keeps = match before {
            Some('i') => !word.before_is(at, "Hi"),
            Some(letter) => matches!(letter, 'a' | 'o' | 'u' | 'è' | 's'),
            None => true,
        };
        if !keeps {
            word.cut(1);
        }
    }
    let Some(suffix) = word.longest_from(&["ion", "ier", "ière", "Ier", "Ière", "e"], rv) else {
        return;
    };
    let start = word.start_of(suffix);
    match suffix {
        "ion" => {
            let after_s_or_t = start
                .checked_sub(1)
                .is_some_and(|at| at >= rv && matches!(word.at(at), Some('s' | 't')));
            if start >= r2 && after_s_or_t {
                word.cut(3);
            }
        }
        "e" => word.cut(1),
        _ => word.replace(suffix.chars().count(), "i"),
    }
}

/// Step 6: `é` or `è` before at least one final non-vowel loses its accent.
fn unaccent(word: &mut Word) {
    let letters = word.letters();
    let tail = letters
        .iter()
        .rev()
        .take_while(|letter| !vowel(**letter))
        .count();
    if tail == 0 {
        return;
    }
    let Some(at) = letters.len().checked_sub(tail.saturating_add(1)) else {
        return;
    };
    if matches!(word.at(at), Some('é' | 'è')) {
        let mut text: Vec<char> = word.letters().to_vec();
        if let Some(letter) = text.get_mut(at) {
            *letter = 'e';
        }
        *word = Word::of(&text.into_iter().collect::<String>());
    }
}

fn postlude(word: &Word) -> String {
    let mut out = String::with_capacity(word.len());
    let mut letters = word.letters().iter().copied().peekable();
    while let Some(letter) = letters.next() {
        match letter {
            'I' => out.push('i'),
            'U' => out.push('u'),
            'Y' => out.push('y'),
            'H' => match letters.peek() {
                Some('e') => {
                    letters.next();
                    out.push('ë');
                }
                Some('i') => {
                    letters.next();
                    out.push('ï');
                }
                _ => {}
            },
            other => out.push(other),
        }
    }
    out
}
