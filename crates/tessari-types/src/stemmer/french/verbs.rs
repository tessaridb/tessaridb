use super::*;

pub(super) const I_VERB: [&str; 35] = [
    "îmes", "ît", "îtes", "i", "ie", "ies", "ir", "ira", "irai", "iraIent", "irais", "irait",
    "iras", "irent", "irez", "iriez", "irions", "irons", "iront", "is", "issaIent", "issais",
    "issait", "issant", "issante", "issantes", "issants", "isse", "issent", "isses", "issez",
    "issiez", "issions", "issons", "it",
];

pub(super) const E_VERB: [&str; 19] = [
    "é", "ée", "ées", "és", "èrent", "er", "era", "erai", "eraIent", "erais", "erait", "eras",
    "erez", "eriez", "erions", "erons", "eront", "ez", "iez",
];

pub(super) const A_VERB: [&str; 17] = [
    "âmes", "ât", "âtes", "a", "ai", "aIent", "ait", "ant", "ante", "antes", "ants", "as", "asse",
    "assent", "asses", "assiez", "assions",
];

/// Step 2a.
pub(super) fn i_verb(word: &mut Word, rv: usize) -> bool {
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
pub(super) fn e_or_a_verb(word: &mut Word, regions: &Regions) -> bool {
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
