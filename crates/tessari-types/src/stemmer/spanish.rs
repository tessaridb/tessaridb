//! The Snowball Spanish stemmer, written from the published algorithm
//! (snowballstem.org/algorithms/spanish), checked against its vocabulary.

use super::word::Word;

fn vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü'
    )
}

/// Whether `letter` is one this stemmer reads.
pub(super) fn reads(letter: char) -> bool {
    letter.is_ascii_lowercase() || matches!(letter, 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü' | 'ñ')
}

const PRONOUNS: [&str; 13] = [
    "me", "se", "sela", "selo", "selas", "selos", "la", "le", "lo", "las", "les", "los", "nos",
];

const STANDARD: [&str; 48] = [
    "anza", "anzas", "ico", "ica", "icos", "icas", "ismo", "ismos", "able", "ables", "ible",
    "ibles", "ista", "istas", "oso", "osa", "osos", "osas", "amiento", "amientos", "imiento",
    "imientos", "adora", "ador", "ación", "adoras", "adores", "aciones", "ante", "antes", "ancia",
    "ancias", "acion", "logía", "logías", "ución", "uciones", "ucion", "encia", "encias", "amente",
    "mente", "idad", "idades", "iva", "ivo", "ivas", "ivos",
];

const Y_VERB: [&str; 12] = [
    "ya", "ye", "yan", "yen", "yeron", "yendo", "yo", "yó", "yas", "yes", "yais", "yamos",
];

const VERB: [&str; 96] = [
    "en", "es", "éis", "emos", "arían", "arías", "arán", "arás", "aríais", "aría", "aréis",
    "aríamos", "aremos", "ará", "aré", "erían", "erías", "erán", "erás", "eríais", "ería", "eréis",
    "eríamos", "eremos", "erá", "eré", "irían", "irías", "irán", "irás", "iríais", "iría", "iréis",
    "iríamos", "iremos", "irá", "iré", "aba", "ada", "ida", "ía", "ara", "iera", "ad", "ed", "id",
    "ase", "iese", "aste", "iste", "an", "aban", "ían", "aran", "ieran", "asen", "iesen", "aron",
    "ieron", "ado", "ido", "ando", "iendo", "ió", "ar", "er", "ir", "as", "abas", "adas", "idas",
    "ías", "aras", "ieras", "ases", "ieses", "ís", "áis", "abais", "íais", "arais", "ierais",
    "aseis", "ieseis", "asteis", "isteis", "ados", "idos", "amos", "ábamos", "íamos", "imos",
    "áramos", "iéramos", "iésemos", "ásemos",
];

/// RV: after the next vowel when the second letter is a consonant, after the
/// next consonant when the first two are vowels, and after the third letter
/// for a consonant then a vowel.
fn rv_of(word: &Word) -> usize {
    let end = word.len();
    let next = |from: usize, wanted: bool| {
        (from..end)
            .find(|at| word.at(*at).is_some_and(vowel) == wanted)
            .map(|at| at.saturating_add(1))
    };
    let (Some(first), Some(second)) = (word.at(0), word.at(1)) else {
        return end;
    };
    let found = match (vowel(first), vowel(second)) {
        (_, false) => next(2, true),
        (true, true) => next(2, false),
        (false, true) => Some(3),
    };
    found.unwrap_or(end).min(end)
}

/// The stem of one lower-case Spanish word.
pub(super) fn stem(text: &str) -> String {
    let mut word = Word::of(text);
    let rv = rv_of(&word);
    let (r1, r2) = word.regions(vowel);
    attached_pronoun(&mut word, rv);
    if !standard(&mut word, r1, r2) && !y_verb(&mut word, rv) {
        verb(&mut word, rv);
    }
    residual(&mut word, rv);
    word.letters()
        .iter()
        .map(|letter| match letter {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' => 'u',
            other => *other,
        })
        .collect()
}

fn attached_pronoun(word: &mut Word, rv: usize) {
    let Some(pronoun) = word.longest(&PRONOUNS) else {
        return;
    };
    let at = word.start_of(pronoun);
    let mut before = word.clone();
    before.cut(pronoun.chars().count());
    let Some(form) = before.longest(&[
        "iéndo", "ándo", "ár", "ér", "ír", "ando", "iendo", "ar", "er", "ir", "yendo",
    ]) else {
        return;
    };
    let form_at = before.start_of(form);
    if form_at < rv {
        return;
    }
    let plain = match form {
        "iéndo" => Some("iendo"),
        "ándo" => Some("ando"),
        "ár" => Some("ar"),
        "ér" => Some("er"),
        "ír" => Some("ir"),
        _ => None,
    };
    if form == "yendo" && !before.before_is(form_at, "u") {
        return;
    }
    word.cut(word.len().saturating_sub(at));
    if let Some(plain) = plain {
        word.replace(form.chars().count(), plain);
    }
}

/// Step 1; whether a suffix was removed.
fn standard(word: &mut Word, r1: usize, r2: usize) -> bool {
    let Some(suffix) = word.longest(&STANDARD) else {
        return false;
    };
    let start = word.start_of(suffix);
    let count = suffix.chars().count();
    let in_r2 = start >= r2;
    match suffix {
        "logía" | "logías" if in_r2 => word.replace(count, "log"),
        "ución" | "uciones" | "ucion" if in_r2 => word.replace(count, "u"),
        "encia" | "encias" if in_r2 => word.replace(count, "ente"),
        "amente" if start >= r1 => {
            word.cut(count);
            if let Some(more) = word.longest(&["iv", "os", "ic", "ad"])
                && word.start_of(more) >= r2
            {
                word.cut(2);
                if more == "iv" && word.ends_with("at") && word.start_of("at") >= r2 {
                    word.cut(2);
                }
            }
        }
        "mente" if in_r2 => {
            word.cut(count);
            if let Some(more) = word.longest(&["ante", "able", "ible"])
                && word.start_of(more) >= r2
            {
                word.cut(4);
            }
        }
        "idad" | "idades" if in_r2 => {
            word.cut(count);
            if let Some(more) = word.longest(&["abil", "ic", "iv"])
                && word.start_of(more) >= r2
            {
                word.cut(more.chars().count());
            }
        }
        "iva" | "ivo" | "ivas" | "ivos" if in_r2 => {
            word.cut(count);
            if word.ends_with("at") && word.start_of("at") >= r2 {
                word.cut(2);
            }
        }
        "adora" | "ador" | "ación" | "adoras" | "adores" | "aciones" | "ante" | "antes"
        | "ancia" | "ancias" | "acion"
            if in_r2 =>
        {
            word.cut(count);
            if word.ends_with("ic") && word.start_of("ic") >= r2 {
                word.cut(2);
            }
        }
        "logía" | "logías" | "ución" | "uciones" | "ucion" | "encia" | "encias" | "amente"
        | "mente" | "idad" | "idades" | "iva" | "ivo" | "ivas" | "ivos" | "adora" | "ador"
        | "ación" | "adoras" | "adores" | "aciones" | "ante" | "antes" | "ancia" | "ancias"
        | "acion" => return false,
        _ if in_r2 => word.cut(count),
        _ => return false,
    }
    true
}

/// Step 2a; whether a suffix was removed.
fn y_verb(word: &mut Word, rv: usize) -> bool {
    let Some(suffix) = word.longest_from(&Y_VERB, rv) else {
        return false;
    };
    if !word.before_is(word.start_of(suffix), "u") {
        return false;
    }
    word.cut(suffix.chars().count());
    true
}

/// Step 2b.
fn verb(word: &mut Word, rv: usize) {
    let Some(suffix) = word.longest_from(&VERB, rv) else {
        return;
    };
    let start = word.start_of(suffix);
    word.cut(suffix.chars().count());
    if matches!(suffix, "en" | "es" | "éis" | "emos") && word.before_is(start, "gu") {
        word.cut(1);
    }
}

/// Step 3.
fn residual(word: &mut Word, rv: usize) {
    let Some(suffix) = word.longest(&["os", "a", "o", "á", "í", "ó", "e", "é"]) else {
        return;
    };
    let start = word.start_of(suffix);
    if start < rv {
        return;
    }
    word.cut(suffix.chars().count());
    if matches!(suffix, "e" | "é") && word.ends_with("gu") && word.start_of("u") >= rv {
        word.cut(1);
    }
}
