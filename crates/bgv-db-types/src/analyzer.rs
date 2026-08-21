//! Turning text into the terms a search matches on.
//!
//! # Why this is a schema thing and not an index thing
//!
//! Every search engine puts the analyzer on the index. Here it belongs to the
//! **field**, and the reason is the one rule this store has applied in every
//! layer: *which access path runs is decided by what exists; the answer is not.*
//! An analyzer on the index would make `body MATCHES 'Lovelace'` find nothing
//! before an index existed and something after — or different things under two
//! indexes — which is the failure a term index was already refused for once.
//!
//! So the analyzer is declared, attached to a field, and used by **both** the
//! scan and the index. The index then makes the same question fast without
//! being able to change its answer, which is what an index is for.
//!
//! # The tokenizer is fixed and the filters are declared
//!
//! Splitting on non-alphanumeric boundaries is what every filter chain assumes
//! underneath it, so making it configurable now would be a knob with one
//! setting. Filters are the part that differs between languages and uses, so
//! those are named — under the rule that has governed every addition here: one
//! is in when the language cannot already say it.

use core::fmt;

/// One step applied to every token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Filter {
    /// Fold to lower case, so `Lovelace` and `lovelace` are one term.
    Lowercase,
    /// Fold the common accented Latin letters to their unaccented forms, so
    /// `café` and `cafe` are one term.
    ///
    /// A table rather than a full Unicode decomposition: the table covers the
    /// letters a Latin-script corpus actually carries, and a real
    /// normalisation is a dependency and a decision of its own.
    Ascii,
}

impl Filter {
    /// Every filter, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Lowercase, Self::Ascii];

    /// How the filter is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lowercase => "lowercase",
            Self::Ascii => "ascii",
        }
    }

    /// The filter a word names, if it names one.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|filter| filter.name().eq_ignore_ascii_case(word))
    }

    /// Apply this filter to one token.
    fn apply(self, token: &str) -> String {
        match self {
            Self::Lowercase => token.to_lowercase(),
            Self::Ascii => token.chars().map(fold).collect(),
        }
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How text becomes terms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analyzer {
    filters: Vec<Filter>,
}

impl Analyzer {
    /// An analyzer applying these filters, in this order.
    ///
    /// Order matters and is the caller's: `ascii` then `lowercase` and
    /// `lowercase` then `ascii` agree for Latin text and need not in general,
    /// so the chain is applied as written rather than sorted into a canonical
    /// form nobody asked for.
    #[must_use]
    pub fn new(filters: Vec<Filter>) -> Self {
        Self { filters }
    }

    /// The filters, in the order they are applied.
    #[must_use]
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// The terms this text holds.
    ///
    /// Split on anything that is not a letter or a digit, then filtered. Empty
    /// tokens never survive, so punctuation contributes nothing.
    #[must_use]
    pub fn terms(&self, text: &str) -> Vec<String> {
        text.split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .map(|token| {
                self.filters
                    .iter()
                    .fold(token.to_owned(), |held, filter| filter.apply(&held))
            })
            .filter(|token| !token.is_empty())
            .collect()
    }
}

/// One accented Latin letter, folded.
///
/// A table rather than a decomposition: it covers what a Latin-script corpus
/// carries, and anything outside it passes through unchanged rather than being
/// dropped — a letter this table does not know is still a letter.
fn fold(character: char) -> char {
    match character {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'È' | 'É' | 'Ê' | 'Ë' => 'E',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'O',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
        'ñ' => 'n',
        'Ñ' => 'N',
        'ç' => 'c',
        'Ç' => 'C',
        'ý' | 'ÿ' => 'y',
        'Ý' => 'Y',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{Analyzer, Filter};

    fn simple() -> Analyzer {
        Analyzer::new(vec![Filter::Lowercase, Filter::Ascii])
    }

    #[test]
    fn text_becomes_the_words_it_holds() {
        assert_eq!(
            simple().terms("Ada Lovelace, 1843!"),
            vec!["ada", "lovelace", "1843"]
        );
    }

    #[test]
    fn punctuation_contributes_no_terms() {
        assert_eq!(simple().terms("  ...  "), Vec::<String>::new());
        assert_eq!(simple().terms(""), Vec::<String>::new());
    }

    #[test]
    fn the_filters_are_what_makes_two_spellings_one_term() {
        assert_eq!(simple().terms("Café"), simple().terms("cafe"));
        assert_eq!(simple().terms("Lovelace"), simple().terms("lovelace"));
    }

    #[test]
    fn an_analyzer_with_no_filters_keeps_the_words_as_written() {
        let bare = Analyzer::default();
        assert_eq!(bare.terms("Ada Lovelace"), vec!["Ada", "Lovelace"]);
    }

    #[test]
    fn a_letter_the_fold_does_not_know_is_still_a_letter() {
        // Dropping it would silently shorten a term and make a search miss.
        let folded = Analyzer::new(vec![Filter::Ascii]);
        assert_eq!(folded.terms("日本語"), vec!["日本語"]);
        // `ó` is in the table and folds; `Ł` and `ź` are not, and survive
        // rather than being dropped.
        assert_eq!(folded.terms("Łódź"), vec!["Łodź"]);
    }

    #[test]
    fn every_filter_is_findable_by_its_own_name() {
        for filter in Filter::ALL {
            assert_eq!(Filter::parse(filter.name()), Some(*filter));
            assert_eq!(Filter::parse(&filter.name().to_uppercase()), Some(*filter));
        }
        assert_eq!(Filter::parse("stemmer"), None);
    }
}
