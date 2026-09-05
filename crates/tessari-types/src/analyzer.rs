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
use core::ops::Range;

/// One token of a text: the bytes it occupied and the term it became.
///
/// The two travel together because a caller that has one and not the other
/// cannot use either: a term without its bytes cannot be pointed at, and bytes
/// without their term cannot be matched against a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The half-open **byte** range this token occupies in the original text.
    ///
    /// Bytes rather than characters because that is what slices a `str`, and
    /// because it is the unit the index format names for the offsets it does not
    /// store — so a stored source could later answer identically.
    pub bytes: Range<usize>,
    /// What the filters turned the token into.
    pub term: String,
}

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
    /// Reduce an English word to the form its relatives share, so `running`,
    /// `runs` and `ran`'s regular cousins become one term.
    ///
    /// The other two filters make two *spellings* of one word meet. This is the
    /// one that makes two *words* meet, which is what a person means by search:
    /// without it, a collection answers `run` with the documents that happen to
    /// spell it that way and silently omits the ones that say `running`.
    ///
    /// Only lower-case ASCII words are stemmed and everything else passes
    /// through unchanged, so a chain that wants stemming writes `lowercase`
    /// before it — see [`stem`](crate::stem) for why half-stemming is worse
    /// than not stemming.
    Stemmer,
}

impl Filter {
    /// Every filter, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[Self::Lowercase, Self::Ascii, Self::Stemmer];

    /// How the filter is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lowercase => "lowercase",
            Self::Ascii => "ascii",
            Self::Stemmer => "stemmer",
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
            Self::Stemmer => crate::stemmer::stem(token),
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
        self.tokens(text, &self.filters)
    }

    /// The **prefixes** this text holds: for each word typed, the spellings a
    /// stored term may begin with.
    ///
    /// One entry per word, and each entry is a small set of alternatives, so a
    /// caller asks *does some stored term begin with any of these*.
    ///
    /// # Why a prefix is analysed differently, and why it needs two spellings
    ///
    /// This module's opening argument is that index-time and query-time analysis
    /// must be identical, and they still are — for a *term*. A prefix is not a
    /// term. It is the beginning of one, and the beginning of a word cannot be
    /// stemmed: `runni` stemmed is `runni`, which is the beginning of nothing,
    /// while the field stores `running` as `run`. Applying the whole chain would
    /// be consistent and would find nothing.
    ///
    /// So the **unstemmed** spelling is one alternative. It is not enough on its
    /// own, and the case that shows why is the one a reader hits first: typing
    /// the *complete* word. `contention` is stored as `content`, and `content`
    /// does not begin with `contention` — so a reader who typed six letters
    /// would find the record and a reader who typed all ten would not. The
    /// **stemmed** spelling is therefore the second alternative, and with it a
    /// complete word is always a prefix of itself.
    ///
    /// That is the property worth stating plainly: `MATCHES PREFIX 'w'` always
    /// reaches at least what `MATCHES 'w'` reaches. Without the second spelling
    /// it does not, and a prefix operator that can find *less* than an exact one
    /// is not something to offer a reader as they type.
    ///
    /// The lowercasing and folding apply to both, because those make two
    /// spellings one and the dictionary holds the folded form.
    ///
    /// One honest limit remains: a prefix longer than the stem and not a word in
    /// its own right reaches nothing. `runni` finds no `running`, because the
    /// store holds `run` and neither spelling of the query begins it. The letters
    /// were never stored, and inventing a match for them would be guessing.
    ///
    /// On a chain with no stemmer the two alternatives coincide and each entry
    /// holds exactly one spelling.
    #[must_use]
    pub fn prefixes(&self, text: &str) -> Vec<Vec<String>> {
        let unstemmed: Vec<Filter> = self
            .filters
            .iter()
            .copied()
            .filter(|filter| *filter != Filter::Stemmer)
            .collect();
        let raw = self.tokens(text, &unstemmed);
        let stemmed = self.tokens(text, &self.filters);
        raw.into_iter()
            .zip(stemmed)
            .map(|(plain, stem)| {
                if plain == stem {
                    vec![plain]
                } else {
                    vec![plain, stem]
                }
            })
            .collect()
    }

    /// The tokens this text holds, each with the bytes it occupies.
    ///
    /// The offset source a highlight marks from. It is the field's own declared
    /// analyzer that assigns these positions, which is what keeps a highlight
    /// answerable without an index: the record's text is already in hand by the
    /// time anything is being marked in it.
    ///
    /// A token whose filters leave it empty is dropped here exactly as it is in
    /// [`terms`](Self::terms) — it became no term, so there is nothing to mark.
    #[must_use]
    pub fn spans(&self, text: &str) -> Vec<Token> {
        self.walk(text, &self.filters)
    }

    /// Split, then fold each token through `filters`.
    ///
    /// **Expressed through [`walk`](Self::walk) rather than beside it.** A second
    /// tokenizer that agrees today is still a second tokenizer, and the drift
    /// would be invisible in the worst way — a highlight a character off, on a
    /// query that still matched. One walk, two projections of it.
    fn tokens(&self, text: &str, filters: &[Filter]) -> Vec<String> {
        self.walk(text, filters)
            .into_iter()
            .map(|token| token.term)
            .collect()
    }

    /// Split on anything that is not a letter or a digit, keeping where each
    /// token was, then fold each through `filters`.
    ///
    /// The positions come from `char_indices`, so a multi-byte character
    /// contributes its real byte width and every range falls on a character
    /// boundary. `Café` occupies five bytes, and a highlight over it covers five.
    fn walk(&self, text: &str, filters: &[Filter]) -> Vec<Token> {
        let mut found = Vec::new();
        let mut start = None;
        for (at, character) in text.char_indices() {
            if character.is_alphanumeric() {
                start.get_or_insert(at);
                continue;
            }
            if let Some(from) = start.take() {
                push(&mut found, text, from..at, filters);
            }
        }
        if let Some(from) = start {
            push(&mut found, text, from..text.len(), filters);
        }
        found
    }
}

/// Fold one token through `filters` and keep it if anything survives.
fn push(into: &mut Vec<Token>, text: &str, bytes: Range<usize>, filters: &[Filter]) {
    let Some(token) = text.get(bytes.clone()) else {
        return;
    };
    let term = filters
        .iter()
        .fold(token.to_owned(), |held, filter| filter.apply(&held));
    if term.is_empty() {
        return;
    }
    into.push(Token { bytes, term });
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
        assert_eq!(Filter::parse("porter"), None);
    }

    #[test]
    fn the_chain_that_makes_two_words_rather_than_two_spellings_meet() {
        // The order is the caller's and it matters: the stemmer is defined over
        // lower-case words, so `lowercase` has to come first or `Running` is
        // returned untouched rather than half-stemmed.
        let full = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii, Filter::Stemmer]);
        assert_eq!(full.terms("Running quickly"), vec!["run", "quick"]);
        assert_eq!(full.terms("He runs"), vec!["he", "run"]);

        // Without the stemmer these are two different terms, which is the whole
        // reason the filter exists.
        assert_ne!(simple().terms("running"), simple().terms("runs"));
        assert_eq!(full.terms("running"), full.terms("runs"));
    }

    /// Whether any stored term begins with any spelling of any typed word.
    ///
    /// The matching rule `MATCHES PREFIX` applies, written here so the tests
    /// assert the rule rather than a re-derivation of it.
    fn reaches(analyzer: &Analyzer, stored: &str, typed: &str) -> bool {
        let terms = analyzer.terms(stored);
        let asked = analyzer.prefixes(typed);
        !asked.is_empty()
            && asked.iter().all(|alternatives| {
                alternatives
                    .iter()
                    .any(|prefix| terms.iter().any(|term| term.starts_with(prefix)))
            })
    }

    #[test]
    fn a_prefix_is_folded_but_never_stemmed() {
        let full = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii, Filter::Stemmer]);
        // The folding applies: a prefix of a lower-cased, unaccented word is
        // what the dictionary holds.
        assert_eq!(full.prefixes("VECto"), vec![vec!["vecto".to_owned()]]);
        assert_eq!(full.prefixes("Café"), vec![vec!["cafe".to_owned()]]);
        // The stemming does not remove the typed spelling — it adds one beside
        // it. `runni` stems to itself, so there is only the one.
        assert_eq!(full.prefixes("runni"), vec![vec!["runni".to_owned()]]);
        assert_eq!(full.terms("running"), vec!["run"]);
        // A prefix longer than the stem and not a word of its own reaches
        // nothing, because the store never held those letters.
        assert!(!reaches(&full, "running", "runni"));
        // A prefix at or below the stem does reach it.
        assert!(reaches(&full, "running", "ru"));
    }

    #[test]
    fn a_complete_word_is_a_prefix_of_itself_even_when_it_stems_to_something_shorter() {
        // The case that makes the second spelling necessary, and the one a
        // reader hits first: typing all of a word rather than most of it.
        let full = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii, Filter::Stemmer]);
        assert_eq!(full.terms("contention"), vec!["content"]);
        assert!(reaches(&full, "Locking and contention", "conten"));
        assert!(reaches(&full, "Locking and contention", "contention"));
        assert!(reaches(&full, "Running a compaction", "running"));
        // Which is the property in general: a prefix reaches at least what an
        // exact term match reaches.
        for word in ["contention", "running", "locking", "compaction"] {
            let stored = "Locking and contention while running a compaction";
            let terms = full.terms(stored);
            let exact = full.terms(word);
            assert!(exact.iter().all(|term| terms.contains(term)), "{word}");
            assert!(reaches(&full, stored, word), "{word}");
        }
    }

    #[test]
    fn a_chain_with_no_stemmer_offers_one_spelling_per_word() {
        // The two alternatives coincide, so on a field that does not stem there
        // is no asymmetry and no second walk to pay for.
        for text in ["Ada Lovelace", "Café au lait", "1843"] {
            let asked = simple().prefixes(text);
            let terms = simple().terms(text);
            assert_eq!(asked.len(), terms.len(), "{text}");
            for (alternatives, term) in asked.iter().zip(&terms) {
                assert_eq!(alternatives, &vec![term.clone()], "{text}");
            }
        }
    }

    #[test]
    fn the_spans_and_the_terms_are_two_readings_of_one_walk() {
        // Asserted directly rather than inferred from a passing highlight,
        // because this is the property the whole offset source rests on: a
        // second tokenizer that agreed today would drift silently.
        let full = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii, Filter::Stemmer]);
        for text in [
            "Ada Lovelace, 1843!",
            "  ...  ",
            "",
            "Running quickly",
            "Café au lait",
            "日本語 and Łódź",
            "trailing",
            "1843",
        ] {
            let walked: Vec<String> = full.spans(text).into_iter().map(|t| t.term).collect();
            assert_eq!(walked, full.terms(text), "{text}");
        }
    }

    #[test]
    fn a_span_covers_the_bytes_the_token_occupied_and_not_the_term_it_became() {
        // The criterion's deciding case in miniature: the reader typed three
        // letters, the text holds seven, and the highlight is over the seven.
        let full = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii, Filter::Stemmer]);
        let text = "He was Running fast";
        let spans = full.spans(text);
        let running = spans
            .iter()
            .find(|token| token.term == "run")
            .expect("the text holds a word that stems to run");
        assert_eq!(running.bytes, 7..14);
        assert_eq!(&text[running.bytes.clone()], "Running");
    }

    #[test]
    fn a_multibyte_character_contributes_its_real_byte_width() {
        // `é` is two bytes, so a span counted in characters would be short by
        // one and the mark would stop mid-letter.
        let folded = Analyzer::new(vec![Filter::Lowercase, Filter::Ascii]);
        let text = "un Café ici";
        let spans = folded.spans(text);
        let cafe = spans
            .iter()
            .find(|token| token.term == "cafe")
            .expect("the text holds a folded café");
        assert_eq!(cafe.bytes, 3..8);
        assert_eq!(&text[cafe.bytes.clone()], "Café");
        // Every range slices, which is the invariant `char_indices` buys.
        for token in &spans {
            assert!(text.get(token.bytes.clone()).is_some(), "{token:?}");
        }
    }

    #[test]
    fn punctuation_opens_no_token_and_so_leaves_no_span_to_mark() {
        // Nothing became a term, so there is nothing a highlight could claim
        // matched — and the spans agree with the terms about that.
        assert!(simple().spans("  ...  ").is_empty());
        assert!(simple().spans("").is_empty());
    }

    #[test]
    fn a_stemmer_without_lowercase_in_front_of_it_leaves_the_word_alone() {
        // Stated as a test rather than a comment: half-stemming would produce a
        // term neither spelling of the word reaches, so the filter declines.
        let bare = Analyzer::new(vec![Filter::Stemmer]);
        assert_eq!(bare.terms("Running"), vec!["Running"]);
        assert_eq!(bare.terms("running"), vec!["run"]);
    }
}
