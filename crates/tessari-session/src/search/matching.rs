//! Whether **this document** holds what the query asked for.
//!
//! Every function here is the **scan**'s answer, and each one has an index
//! counterpart that must agree with it record for record (ADR-0046). Keeping
//! them together is what makes that obligation visible: a fourth operator added
//! below is a fourth operator owing the same equality test.

use tessari_types::{Analyzer, Value, within_edits};

use tessari_constants::{SEARCH_FUZZY_MAX_EDITS, SEARCH_FUZZY_PREFIX};

use super::query::{Asked, asked};

/// Whether `asked` appears in `held` in order, within `slop` extra tokens.
///
/// Order is the whole difference between a phrase and a conjunction, and the
/// reason the fixture that tests it holds the same two terms in both orders:
/// `ada lovelace` and `lovelace ada` carry identical terms with identical
/// frequencies, so every test written against conjunction passes on either.
///
/// The span of a run of `n` terms is `n - 1` when they are adjacent, so the
/// admissible span is `n - 1 + slop` and **slop 0 is contiguity** — the two
/// cases are one rule rather than a special case and a general one, which is
/// what keeps them from drifting apart.
///
/// For a fixed start the *earliest* later occurrence of each term minimises the
/// span, so a greedy walk decides the whole question and no backtracking is
/// needed.
fn holds_run(held: &[String], asked: &[String], slop: usize) -> bool {
    let Some(first) = asked.first() else {
        return false;
    };
    let limit = asked.len().saturating_sub(1).saturating_add(slop);
    held.iter().enumerate().any(|(start, token)| {
        if token != first {
            return false;
        }
        let mut at = start;
        for term in &asked[1..] {
            let Some(found) = held
                .iter()
                .skip(at.saturating_add(1))
                .position(|held| held == term)
            else {
                return false;
            };
            at = at.saturating_add(1).saturating_add(found);
        }
        at.saturating_sub(start) <= limit
    })
}

/// Whether the analyzed text answers the query — as a phrase when it is quoted,
/// and as a conjunction when it is not.
///
/// **Every** term, because "find me documents about X Y" means both — and a
/// field with no analyzer holds no terms, so it matches nothing rather than
/// failing.
///
/// A **quoted** query means the words in that order and adjacent. It is answered
/// here, exactly, without an index and without positions: the analyzer is a
/// property of the *field* rather than of an index, so this function already
/// holds the record's ordered token list, and a token's ordinal is its index in
/// that list. Positions on an index are therefore an access path and not a
/// capability — the same relationship the term dictionary has to `MATCHES` — so
/// a phrase on a field nobody declared `positions` for is answered rather than
/// refused (ADR-0046, applied without amendment).
///
/// One token in, one token out is what makes the ordinals line up: `lowercase`,
/// `ascii` and `stemmer` each map a token to exactly one token. A filter that
/// split or dropped one would move a phrase's meaning silently, which is why
/// the tests assert that property directly rather than inferring it from a
/// passing query.
///
/// An **unquoted** query is boolean: every group must be answered by one of its
/// terms and no excluded term may be held. A query with no `OR` and no `NOT` is
/// that with every group a single term, which is the conjunction this operator
/// always meant — so the boolean forms widen the language without moving what
/// an existing query asks.
pub(crate) fn matches_terms(analyzer: Option<&Analyzer>, held: &Value, wanted: &Value) -> bool {
    let (Some(analyzer), Value::String(text), Value::String(query)) = (analyzer, held, wanted)
    else {
        return false;
    };
    let terms = analyzer.terms(text);
    match asked(analyzer, query) {
        Asked::Phrase { terms: run, slop } => holds_run(&terms, &run, slop),
        Asked::Boolean { required, excluded } => {
            !required.is_empty()
                && required
                    .iter()
                    .all(|group| group.iter().any(|term| terms.contains(term)))
                && !excluded.iter().any(|term| terms.contains(term))
        }
    }
}

/// Whether the analyzed text holds a term beginning with every prefix of the
/// query.
///
/// **Every** prefix and **a** term: the conjunction is across what was typed and
/// the disjunction is within each word, which is what "documents about vecto…
/// and lo…" means. The same shape `MATCHES` has, one level looser.
///
/// The query is analysed with [`Analyzer::prefixes`] rather than
/// [`Analyzer::terms`] — the beginning of a word is not a word, and stemming it
/// produces the beginning of nothing. That function carries the argument.
///
/// This is the **scan**'s answer. The index reaches the same set through the term
/// dictionary, and the two are asserted to agree record for record, exactly as
/// they are for `MATCHES`.
pub(crate) fn matches_prefix_terms(
    analyzer: Option<&Analyzer>,
    held: &Value,
    wanted: &Value,
) -> bool {
    let (Some(analyzer), Value::String(text), Value::String(query)) = (analyzer, held, wanted)
    else {
        return false;
    };
    let terms = analyzer.terms(text);
    let asked = analyzer.prefixes(query);
    !asked.is_empty()
        && asked.iter().all(|alternatives| {
            alternatives
                .iter()
                .any(|prefix| terms.iter().any(|term| term.starts_with(prefix)))
        })
}

/// Whether analyzed text holds, for **every** word typed, a term within the edit
/// budget that shares that word's mandatory non-fuzzy prefix.
///
/// The same two levels as the two operators above — a conjunction across the
/// words, a disjunction within each — one level looser again.
///
/// # The prefix is here because it is meaning, not because it is fast
///
/// This function has no index and no dictionary to walk, so a leading run of
/// characters buys it nothing at all. It applies the restriction anyway, and
/// that is the point: `SEARCH_FUZZY_PREFIX` is part of what `MATCHES FUZZY`
/// asks, so both access paths must apply it or the same statement answers
/// differently depending on whether an index happens to exist (ADR-0046).
///
/// The visible consequence, which `docs/tessariql.md` states rather than leaving
/// to be discovered: a mistake in the first `SEARCH_FUZZY_PREFIX` characters is
/// not found. `xector` does not reach `vector`.
///
/// # Which spelling the distance is measured from
///
/// Both, and a term matching either satisfies the word. The dictionary holds
/// stemmed terms, and a misspelling does not stem where its correct spelling
/// does — `containr` and `container` need not land near each other once a
/// stemmer has had them. Measuring only from the typed word would miss the
/// stored stem; measuring only from the stem would measure a distance between
/// two things the reader never wrote. [`Analyzer::prefixes`] already produces
/// exactly this pair, which is why it is reused here rather than a third
/// analysis being invented.
pub(crate) fn matches_fuzzy_terms(
    analyzer: Option<&Analyzer>,
    held: &Value,
    wanted: &Value,
) -> bool {
    let (Some(analyzer), Value::String(text), Value::String(query)) = (analyzer, held, wanted)
    else {
        return false;
    };
    let terms = analyzer.terms(text);
    let asked = analyzer.prefixes(query);
    !asked.is_empty()
        && asked.iter().all(|alternatives| {
            alternatives.iter().any(|spelling| {
                let leading: String = spelling.chars().take(SEARCH_FUZZY_PREFIX).collect();
                terms.iter().any(|term| {
                    term.starts_with(&leading)
                        && within_edits(spelling, term, SEARCH_FUZZY_MAX_EDITS)
                })
            })
        })
}
