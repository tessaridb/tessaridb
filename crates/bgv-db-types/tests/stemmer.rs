//! The stemmer against the published algorithm's own worked examples.
//!
//! Every case here is one the Porter2 definition names, either as a step's
//! illustration or as an entry in its exception tables. That makes this a check
//! against the *definition*. The complete check is the Snowball project's
//! thirty-thousand-word vocabulary paired with its reference output, which is
//! owed and is not in this repository yet — so a case not listed here is not a
//! case this file has verified.

use bgv_db_types::stem;

/// Assert a whole table at once, so a failure names the word rather than a line.
fn each(pairs: &[(&str, &str)]) {
    let wrong: Vec<String> = pairs
        .iter()
        .filter_map(|(word, expected)| {
            let found = stem(word);
            (found != *expected).then(|| format!("{word} → {found}, expected {expected}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_gener_prefix_keeps_general_and_generate_apart() {
    // The published reason the prefix exception exists: without it R1 falls so
    // early that these two words collapse into one term.
    each(&[
        ("generate", "generat"),
        ("generates", "generat"),
        ("generated", "generat"),
        ("generating", "generat"),
        ("general", "general"),
        ("generally", "general"),
        ("generic", "generic"),
        ("generically", "generic"),
        ("generous", "generous"),
        ("generously", "generous"),
    ]);
}

#[test]
fn a_past_tense_comes_off_and_the_stem_is_repaired() {
    each(&[
        ("plastered", "plaster"),
        ("motoring", "motor"),
        ("conflated", "conflat"),
        ("troubled", "troubl"),
        ("sized", "size"),
        ("filing", "file"),
    ]);
}

#[test]
fn a_doubled_letter_is_undoubled_only_when_english_doubled_it() {
    // `pp` and `nn` are inflectional doubles and come apart; `ll`, `ss` and `zz`
    // are part of the word and stay. Undoubling all of them would turn `fall`
    // into `fal`, which no other form of the word reaches.
    each(&[
        ("hopping", "hop"),
        ("tanned", "tan"),
        ("falling", "fall"),
        ("hissing", "hiss"),
        ("fizzed", "fizz"),
        ("failing", "fail"),
    ]);
}

#[test]
fn a_word_with_no_vowel_before_the_ending_keeps_it() {
    // `bled` is not the past tense of `bl`, and `sing` is not the act of s-ing.
    each(&[("bled", "bled"), ("sing", "sing"), ("feed", "feed")]);
    // `agreed` does have a vowel before it, so it loses one `e` and then the
    // other in step 5.
    each(&[("agreed", "agre")]);
}

#[test]
fn a_trailing_s_needs_a_vowel_two_letters_back() {
    // The rule is not "ends in s"; `gas` and `this` are whole words.
    each(&[
        ("gaps", "gap"),
        ("gas", "gas"),
        ("this", "this"),
        ("bus", "bus"),
        ("class", "class"),
    ]);
}

#[test]
fn a_short_word_keeps_a_longer_stem_than_a_long_one() {
    // `ties` cut to `ti` would share a term with nothing; `cries` to `cri` is
    // the form `cried` and `crying` also reach.
    each(&[("ties", "tie"), ("cries", "cri"), ("skies", "sky")]);
}

#[test]
fn the_irregular_table_wins_over_every_rule() {
    each(&[
        ("skis", "ski"),
        ("dying", "die"),
        ("lying", "lie"),
        ("tying", "tie"),
        ("idly", "idl"),
        ("gently", "gentl"),
        ("ugly", "ugli"),
        ("early", "earli"),
        ("only", "onli"),
        ("singly", "singl"),
    ]);
}

#[test]
fn a_word_that_merely_looks_inflected_is_left_alone() {
    each(&[
        ("sky", "sky"),
        ("news", "news"),
        ("atlas", "atlas"),
        ("cosmos", "cosmos"),
        ("bias", "bias"),
        ("andes", "andes"),
        ("inning", "inning"),
        ("outing", "outing"),
        ("canning", "canning"),
        ("herring", "herring"),
        ("earring", "earring"),
        ("proceed", "proceed"),
        ("exceed", "exceed"),
        ("succeed", "succeed"),
    ]);
}

#[test]
fn the_words_a_search_actually_needs_to_meet() {
    // The point of the whole filter: these are the pairs a person expects one
    // query to find, and without a stemmer none of them meet.
    for (left, right) in [
        ("running", "runs"),
        ("consigned", "consigning"),
        ("happy", "happiness"),
        ("national", "nationally"),
        ("relational", "relate"),
    ] {
        assert_eq!(
            stem(left),
            stem(right),
            "{left} and {right} should share a term, got {} and {}",
            stem(left),
            stem(right)
        );
    }
}

#[test]
fn anything_that_is_not_a_lower_case_ascii_word_passes_through_untouched() {
    // Half-stemming is worse than not stemming: `Running` stripped to `Runn`
    // is a term neither spelling of the word reaches. A chain that wants
    // stemming declares `lowercase` first.
    each(&[
        ("Running", "Running"),
        ("RUNNING", "RUNNING"),
        ("日本語", "日本語"),
        ("Łódź", "Łódź"),
        ("h2o", "h2o"),
        ("", ""),
    ]);
}

#[test]
fn a_word_too_short_to_have_a_suffix_is_returned_as_it_is() {
    each(&[("a", "a"), ("in", "in"), ("as", "as"), ("is", "is")]);
}

#[test]
fn stemming_a_stem_usually_changes_nothing_further() {
    for word in [
        "generate",
        "generically",
        "plastered",
        "hopping",
        "troubled",
        "cries",
        "happiness",
        "nationally",
        "relational",
    ] {
        let once = stem(word);
        assert_eq!(stem(&once), once, "{word} → {once} → {}", stem(&once));
    }
}

#[test]
fn stemming_is_not_idempotent_and_that_is_the_algorithm_rather_than_a_defect() {
    // `agreed` → `agre` → `agr`. The second pass is not re-stemming a word — it
    // is stemming `agre`, which the rules see as a word ending in `e` after a
    // region boundary, and step 5 takes that `e` off.
    //
    // Pinned deliberately, so that nobody later reads the near-idempotence above
    // as a promise and "fixes" the stemmer to satisfy it. A stemmer is a
    // function from words to terms, applied exactly once, at index time and at
    // query time by the same chain — its behaviour on its own output is not a
    // property anything depends on.
    assert_eq!(stem("agreed"), "agre");
    assert_eq!(stem("agre"), "agr");
}
