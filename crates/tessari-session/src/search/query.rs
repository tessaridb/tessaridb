//! What the query string asked for, before any record or index is consulted.
//!
//! Everything here reads the text a caller wrote and nothing else. That is the
//! seam: a question about the *query* cannot depend on the schema, the catalog
//! or whether an index exists, and keeping it in its own module is what makes
//! that hard to violate by accident.

use tessari_types::Analyzer;

/// The word that unions the term beside it into the group before it.
const OR: &str = "OR";

/// The word that excludes the term after it.
const NOT: &str = "NOT";

/// What ends a word, or a quoted phrase, whose last word is a prefix.
const STAR: &str = "*";

/// One word of a query: a term the record must hold, or the beginning of one.
///
/// A prefix carries its alternatives — the spelling as typed and its stem, from
/// [`Analyzer::prefixes`] — because a stored term beginning with either answers
/// it, exactly as for `MATCHES PREFIX` (ADR-0104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Word {
    /// This term, whole.
    Term(String),
    /// A term beginning with one of these.
    Prefix(Vec<String>),
}

impl Word {
    /// Whether one stored term answers this word.
    pub(crate) fn answers(&self, held: &str) -> bool {
        match self {
            Self::Term(term) => term == held,
            Self::Prefix(alternatives) => super::matching::begins(alternatives, held),
        }
    }
}

/// What one `MATCHES` query asks for.
///
/// A **shape** rather than a list of terms, because an inverted index answers
/// the two forms with different reads — an intersection for a conjunction, a
/// union inside each group for a disjunction — and the predicate has to agree
/// with whichever one ran. The list this replaced could say *which* terms were
/// asked for and not *how*, so the two sides could only have agreed by both
/// choosing the same interpretation, which is the arrangement that already
/// failed once (see [`asked`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Asked {
    /// The words in this order, within `slop` extra tokens.
    Phrase {
        /// The phrase's words, in the order they were written. Only the last
        /// can be a prefix, and only when the phrase was starred.
        words: Vec<Word>,
        /// How many extra tokens the run may absorb.
        slop: usize,
    },
    /// Every group answered by at least one of its words, and no excluded word
    /// held.
    ///
    /// A plain conjunction is this with every group a single term, which is why
    /// there is no third variant: `ada lovelace` and `ada OR lovelace` differ in
    /// how the terms are grouped and in nothing else.
    Boolean {
        /// One group per `OR`-joined run; all groups must be answered.
        required: Vec<Vec<Word>>,
        /// Words no matching record may hold.
        excluded: Vec<Word>,
    },
}

impl Asked {
    /// Every prefix this query holds, as its alternatives.
    pub(crate) fn prefixes(&self) -> Vec<&[String]> {
        let words: Vec<&Word> = match self {
            Self::Phrase { words, .. } => words.iter().collect(),
            Self::Boolean { required, excluded } => {
                required.iter().flatten().chain(excluded).collect()
            }
        };
        words
            .into_iter()
            .filter_map(|word| match word {
                Word::Prefix(alternatives) => Some(alternatives.as_slice()),
                Word::Term(_) => None,
            })
            .collect()
    }
}

/// The inside of a quoted phrase, when the query is one.
///
/// Recognised on the **raw** query string, before analysis, because by the time
/// terms exist the quotes are gone: the tokenizer splits on anything that is not
/// alphanumeric and drops empty tokens, so punctuation contributes nothing. A
/// phrase asked of `Analyzer::terms` is indistinguishable from the same words
/// unquoted — which is exactly what this store did until it did not, and the
/// failure was silent in the worst way, answering plausible records in the wrong
/// order rather than answering nothing.
///
/// Both quotes are required. A string with one is a string with one, not a
/// phrase somebody half-typed, and guessing which they meant would make the
/// operator's meaning depend on a typo.
///
/// A trailing `~n` declares **slop**: how many extra tokens the run may absorb.
/// It lives inside the string because that is where the phrase already lives,
/// so no grammar changes and a phrase stays one value a caller can build. `~0`
/// and no marker at all are the same query, which is the property that makes
/// "exact phrase is slop 0" true by construction rather than by convention.
///
/// A trailing `*` instead makes the phrase's last word a prefix (ADR-0104).
///
/// A malformed marker — `~`, `~-1`, `~x` — is **not** silently treated as 0.
/// It is not a phrase at all, so the query falls back to conjunction, which is
/// what an unparseable phrase already meant before this wave.
pub(super) fn phrase_of(query: &str) -> Option<(&str, usize, bool)> {
    let trimmed = query.trim();
    let rest = trimmed.strip_prefix('"')?;
    // Split at the LAST quote, so a phrase may contain one.
    let (inner, tail) = rest.rsplit_once('"')?;
    if tail.is_empty() {
        return Some((inner, 0, false));
    }
    // `"ada lov"*`: the phrase's last word is a prefix (ADR-0104).
    if tail == STAR {
        return Some((inner, 0, true));
    }
    let slop = tail.strip_prefix('~')?.parse::<usize>().ok()?;
    Some((inner, slop, false))
}

/// The malformed slop marker in a query, when somebody tried to write one.
///
/// Separate from [`phrase_of`] because the two answer different questions: that
/// one asks *is this a phrase*, this one asks *did somebody mean one and get the
/// marker wrong*. A string with no opening quote is not an attempt at either.
pub(super) fn malformed_slop(query: &str) -> Option<&str> {
    let trimmed = query.trim();
    let (_, tail) = trimmed.strip_prefix('"')?.rsplit_once('"')?;
    if tail.is_empty() || tail == STAR {
        return None;
    }
    match tail.strip_prefix('~').map(str::parse::<usize>) {
        Some(Ok(_)) => None,
        _ => Some(tail),
    }
}

/// What a query asks for, whatever shape the query has.
///
/// **One function, because the scan and the index must agree on this.** The
/// index generates candidates from what this returns and the predicate then
/// refines them, so a query the two analyse differently is a query the index
/// answers empty while the scan answers correctly — a divergence that depends on
/// whether an index happens to exist, which is exactly what ADR-0046 forbids.
///
/// That divergence was not hypothetical: computing this in two places meant a
/// slop marker tokenized into a term of its own on the index side. `~0` asked
/// the dictionary for a term `0`, no record held one, and the candidate set was
/// empty — so `MATCHES '"ada lovelace"~0'` answered nothing with an index and
/// correctly with none.
///
/// # The operators are read from the raw string, and are uppercase
///
/// `OR` and `NOT` are recognised before analysis, on the words as written, for
/// the same reason the quotes are: the tokenizer keeps only alphanumerics and
/// stems what is left, so by the time terms exist an operator is a term like any
/// other. Requiring them uppercase is what keeps `salt or pepper` meaning three
/// words — a query written before this existed still asks what it asked.
///
/// A word that analyses to nothing contributes nothing and does not consume a
/// pending operator, so `ada OR --- lovelace` is still one group.
pub(crate) fn asked(analyzer: &Analyzer, query: &str) -> Asked {
    if let Some((inner, slop, starred)) = phrase_of(query) {
        return Asked::Phrase {
            words: words_of(analyzer, inner, starred),
            slop,
        };
    }
    let mut required: Vec<Vec<Word>> = Vec::new();
    let mut excluded = Vec::new();
    let mut joining = false;
    let mut negating = false;
    for word in query.split_whitespace() {
        match word {
            OR => joining = true,
            NOT => negating = true,
            _ => {
                let words = typed(analyzer, word);
                if words.is_empty() {
                    continue;
                }
                if negating {
                    excluded.extend(words);
                } else if let (true, Some(group)) = (joining, required.last_mut()) {
                    group.extend(words);
                } else {
                    required.extend(words.into_iter().map(|word| vec![word]));
                }
                joining = false;
                negating = false;
            }
        }
    }
    Asked::Boolean { required, excluded }
}

/// The words of one whitespace-separated piece of a query: its last token a
/// prefix when the piece ends in `*`, whole terms otherwise.
pub(crate) fn typed(analyzer: &Analyzer, piece: &str) -> Vec<Word> {
    match piece.strip_suffix(STAR) {
        Some(stem) => words_of(analyzer, stem, true),
        None => words_of(analyzer, piece, false),
    }
}

/// The tokens of `text` as words, the last one a prefix when `starred`.
///
/// Unstarred, this is [`Analyzer::terms`] and nothing else, so a query with no
/// star asks exactly what it always asked. Starred, the last token is read the
/// way `MATCHES PREFIX` reads a word ([`Analyzer::prefixes`]) and the ones
/// before it stay terms.
fn words_of(analyzer: &Analyzer, text: &str, starred: bool) -> Vec<Word> {
    let mut words: Vec<Word> = analyzer.terms(text).into_iter().map(Word::Term).collect();
    if starred
        && let Some(alternatives) = analyzer.prefixes(text).pop()
        && let Some(last) = words.last_mut()
    {
        *last = Word::Prefix(alternatives);
    }
    words
}

/// Every word of a query a score weighs, in the order written and with repeats.
///
/// The query read as words rather than as a shape: a score weighs what the
/// record holds of every word written, whatever `OR` or quotes did to which
/// records were matched — the rule `search::score` has always followed — and a
/// starred word is weighed as the one prefix it is.
pub(crate) fn scored_words(analyzer: &Analyzer, query: &str) -> Vec<Word> {
    query
        .split_whitespace()
        .flat_map(|piece| typed(analyzer, piece))
        .collect()
}

/// Whether the query excludes terms and requires none.
///
/// A negation names the **complement** of a posting list, and an inverted index
/// enumerates presence: there is no candidate set for `NOT babbage`, only every
/// record in the table. So the honest plans are a full scan or a refusal, and
/// this store refuses — the same choice `docs/tessariql.md` already makes for a
/// score over a field with no search index, and for the same reason. A statement
/// that did not run beats one that ran over the whole table because a word was
/// spelled `NOT`.
///
/// Read **lexically**, with no analyzer, so the refusal can be raised before the
/// catalog is read and cannot come to depend on the schema. That is safe here in
/// a way it would not be for [`asked`]: this asks whether the query is
/// well-formed, which no index can answer differently, rather than what the
/// query means, which both access paths must answer the same way.
pub(super) fn negation_without_term(query: &str) -> bool {
    if phrase_of(query).is_some() {
        return false;
    }
    let mut excludes = false;
    let mut requires = false;
    let mut negating = false;
    for word in query.split_whitespace() {
        match word {
            OR => {}
            NOT => {
                excludes = true;
                negating = true;
            }
            _ => {
                requires |= !negating;
                negating = false;
            }
        }
    }
    excludes && !requires
}
