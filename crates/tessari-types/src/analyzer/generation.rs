//! Which tokenizer built an index's terms (G058 C3, Q-911).
//!
//! A term index holds what the analyzer made of each record **when the entry
//! was written**. When the code that makes terms changes — the tokenizer splits
//! differently, a filter folds or stems differently — every entry written before
//! is a term the current analyzer would not produce, and a read through it
//! answers a subset of what the scan answers with nothing in an error state.
//! That happened once: `0.22.0-beta` made each Chinese or Japanese ideograph a
//! token of its own, and an index written before still held whole sentences.
//!
//! So the generation is a number the index records when it is built, and a read
//! compares it with this one. The golden test below pins what the current
//! generation produces; changing any of its expected terms is changing the
//! generation, and the constant moves with it.

/// The tokenizer and filter behaviour this build writes terms with.
///
/// `1` is everything before `0.22.0-beta` (an ideograph run was one token);
/// `2` is from `0.22.0-beta`. An index records the generation that built it from
/// `0.26.0-beta`; one written earlier recorded none.
pub const TOKENIZER_GENERATION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::TOKENIZER_GENERATION;
    use crate::{Analyzer, Filter, Language};

    /// Every branch of the tokenizer and every filter, with the terms this
    /// generation makes of them. A change to any line here changes what an index
    /// written before it holds — bump [`TOKENIZER_GENERATION`] with it.
    #[test]
    fn this_generation_makes_these_terms() {
        assert_eq!(
            TOKENIZER_GENERATION, 2,
            "the golden terms below are generation 2's"
        );
        let plain = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii]);
        let cases: [(&Analyzer, &str, &[&str]); 4] = [
            (&plain, "Ada Lovelace, 1843!", &["ada", "lovelace", "1843"]),
            (&plain, "Café Ünïcode naïve", &["cafe", "unicode", "naive"]),
            (
                &plain,
                "東京都 ひらがな",
                &["東", "京", "都", "ひ", "ら", "が", "な"],
            ),
            (&plain, "カタカナ 한국어", &["カタカナ", "한국어"]),
        ];
        for (analyzer, text, expected) in cases {
            assert_eq!(analyzer.terms(text), expected, "tokenizing {text:?}");
        }
        let stems = [
            (Language::English, "running runs", vec!["run", "run"]),
            (Language::Russian, "важные", vec!["важн"]),
            (Language::German, "Häuser", vec!["haus"]),
            (Language::French, "chevaux", vec!["cheval"]),
            (Language::Spanish, "canciones", vec!["cancion"]),
        ];
        for (language, text, expected) in stems {
            let stemming = Analyzer::new(vec![
                Filter::Lowercase,
                Filter::Ascii,
                Filter::Stemmer(language),
            ]);
            assert_eq!(
                stemming.terms(text),
                expected,
                "stemming {text:?} ({language:?})"
            );
        }
    }
}
