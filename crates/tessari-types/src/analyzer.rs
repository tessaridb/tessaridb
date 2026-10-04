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

mod generation;
pub use generation::TOKENIZER_GENERATION;

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
    /// Only lower-case words of the language are stemmed and everything else
    /// passes through unchanged, so a chain that wants stemming writes
    /// `lowercase` before it — see [`stem`](crate::stem) for why half-stemming
    /// is worse than not stemming.
    ///
    /// `stemmer` is English; `stemmer(russian)`, `stemmer(german)`,
    /// `stemmer(french)` and `stemmer(spanish)` name the others (G051 T7.3).
    Stemmer(Language),
}

/// The language a [`Filter::Stemmer`] reduces words of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    /// Porter2.
    English,
    /// Snowball Russian.
    Russian,
    /// Snowball German.
    German,
    /// Snowball French.
    French,
    /// Snowball Spanish.
    Spanish,
}

impl Filter {
    /// Every filter, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::Lowercase,
        Self::Ascii,
        Self::Stemmer(Language::English),
        Self::Stemmer(Language::Russian),
        Self::Stemmer(Language::German),
        Self::Stemmer(Language::French),
        Self::Stemmer(Language::Spanish),
    ];

    /// How the filter is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lowercase => "lowercase",
            Self::Ascii => "ascii",
            // English keeps the spelling it has always been stored under, so
            // an analyzer declared before the languages reads back unchanged.
            Self::Stemmer(Language::English) => "stemmer",
            Self::Stemmer(Language::Russian) => "stemmer(russian)",
            Self::Stemmer(Language::German) => "stemmer(german)",
            Self::Stemmer(Language::French) => "stemmer(french)",
            Self::Stemmer(Language::Spanish) => "stemmer(spanish)",
        }
    }

    /// The filter a word names, if it names one — `stemmer(english)` being a
    /// second spelling of `stemmer`.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        if word.eq_ignore_ascii_case("stemmer(english)") {
            return Some(Self::Stemmer(Language::English));
        }
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
            Self::Stemmer(language) => crate::stemmer::stem_in(language, token),
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
        let raw = self.surfaces(text);
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

    /// The **surface forms** this text holds: the chain without its stemmers,
    /// one for each of [`terms`](Self::terms) and in the same order.
    ///
    /// The raw companion of a stemmed field. A stem is not a spelling anybody
    /// typed, so a misspelling is measured against what the text actually
    /// said — `trasnactoin` is two letters from `transaction` and five from the
    /// `transact` it stems to. One for one because a stemmer maps a token to
    /// exactly one token, so the two lists line up by position.
    #[must_use]
    pub fn surfaces(&self, text: &str) -> Vec<String> {
        if !self.stems() {
            return self.terms(text);
        }
        let unstemmed: Vec<Filter> = self
            .filters
            .iter()
            .copied()
            .filter(|filter| !matches!(filter, Filter::Stemmer(_)))
            .collect();
        self.tokens(text, &unstemmed)
    }

    /// [`terms`](Self::terms) and [`surfaces`](Self::surfaces) of one text
    /// together, each token's analysis looked up in `memo` before it is run.
    ///
    /// For a read that analyses many records of one collection: their words
    /// repeat, and the stemmer is most of what analysis costs, so a token is
    /// stemmed once per read rather than once per occurrence. The answer is the
    /// same two lists the two functions give; `memo` only remembers it, keyed
    /// by the token as written, and is only valid for this analyzer.
    pub fn analysed(&self, text: &str, memo: &mut Memo) -> (Vec<String>, Vec<String>) {
        let mut terms = Vec::new();
        let mut surfaces = Vec::new();
        for bytes in split(text) {
            let Some(token) = text.get(bytes) else {
                continue;
            };
            let (term, surface) = match memo.held.get(token) {
                Some(known) => known.clone(),
                None => {
                    let fold = |filters: &mut dyn Iterator<Item = &Filter>| {
                        filters.fold(token.to_owned(), |held, filter| filter.apply(&held))
                    };
                    let term = fold(&mut self.filters.iter());
                    let surface = fold(
                        &mut self
                            .filters
                            .iter()
                            .filter(|filter| !matches!(filter, Filter::Stemmer(_))),
                    );
                    memo.held
                        .insert(token.to_owned(), (term.clone(), surface.clone()));
                    (term, surface)
                }
            };
            if term.is_empty() {
                continue;
            }
            terms.push(term);
            surfaces.push(surface);
        }
        (terms, surfaces)
    }

    /// Whether the chain holds a stemmer — whether a surface can differ from
    /// its term at all.
    #[must_use]
    pub fn stems(&self) -> bool {
        self.filters
            .iter()
            .any(|filter| matches!(filter, Filter::Stemmer(_)))
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
        for bytes in split(text) {
            push(&mut found, text, bytes, filters);
        }
        found
    }
}

/// Remembered analyses for [`Analyzer::analysed`]: each token as written, with
/// the term and the surface it became.
#[derive(Debug, Default)]
pub struct Memo {
    held: std::collections::HashMap<String, (String, String)>,
}

/// Where each token of `text` lies: runs of letters and digits, and each
/// ideograph alone — the one tokenizer every analysis uses.
fn split(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = None;
    for (at, character) in text.char_indices() {
        if ideograph(character) {
            if let Some(from) = start.take() {
                found.push(from..at);
            }
            found.push(at..at.saturating_add(character.len_utf8()));
            continue;
        }
        if character.is_alphanumeric() {
            start.get_or_insert(at);
            continue;
        }
        if let Some(from) = start.take() {
            found.push(from..at);
        }
    }
    if let Some(from) = start {
        found.push(from..text.len());
    }
    found
}

/// Whether a character is a token by itself: a Han ideograph or a Hiragana
/// letter.
///
/// Chinese and Japanese write words without spaces, so splitting on non-letters
/// made a whole sentence one term and nothing inside it could be found. A
/// segmenting tokenizer needs a dictionary per language; one token per
/// character needs none, and a quoted phrase of characters is then an exact
/// run — `"東京"` holds in `東京都` and not in `京東` — through the same
/// positions every phrase uses. No n-gram is involved: each character is
/// one token at one position, which is what a word is to the index.
///
/// Katakana and Hangul keep their runs: Katakana spells one word per run and
/// Korean separates its words with spaces.
fn ideograph(character: char) -> bool {
    matches!(
        character,
        '\u{3005}'..='\u{3007}'
            | '\u{3040}'..='\u{309F}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FA1F}'
    )
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
mod tests;
