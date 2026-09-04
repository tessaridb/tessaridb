//! What the query string asked for, before any record or index is consulted.
//!
//! Everything here reads the text a caller wrote and nothing else. That is the
//! seam: a question about the *query* cannot depend on the schema, the catalog
//! or whether an index exists, and keeping it in its own module is what makes
//! that hard to violate by accident.

use tessari_types::Analyzer;

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
/// A malformed marker — `~`, `~-1`, `~x` — is **not** silently treated as 0.
/// It is not a phrase at all, so the query falls back to conjunction, which is
/// what an unparseable phrase already meant before this wave.
pub(super) fn phrase_of(query: &str) -> Option<(&str, usize)> {
    let trimmed = query.trim();
    let rest = trimmed.strip_prefix('"')?;
    // Split at the LAST quote, so a phrase may contain one.
    let (inner, tail) = rest.rsplit_once('"')?;
    if tail.is_empty() {
        return Some((inner, 0));
    }
    let slop = tail.strip_prefix('~')?.parse::<usize>().ok()?;
    Some((inner, slop))
}

/// The malformed slop marker in a query, when somebody tried to write one.
///
/// Separate from [`phrase_of`] because the two answer different questions: that
/// one asks *is this a phrase*, this one asks *did somebody mean one and get the
/// marker wrong*. A string with no opening quote is not an attempt at either.
pub(super) fn malformed_slop(query: &str) -> Option<&str> {
    let trimmed = query.trim();
    let (_, tail) = trimmed.strip_prefix('"')?.rsplit_once('"')?;
    if tail.is_empty() {
        return None;
    }
    match tail.strip_prefix('~').map(str::parse::<usize>) {
        Some(Ok(_)) => None,
        _ => Some(tail),
    }
}

/// The terms a query asks for, whatever shape the query has.
///
/// **One function, because the scan and the index must agree on this.** The
/// index generates candidates from these terms and the predicate then refines
/// them, so a query the two analyse differently is a query the index answers
/// empty while the scan answers correctly — a divergence that depends on
/// whether an index happens to exist, which is exactly what ADR-0046 forbids.
///
/// That divergence was not hypothetical: computing this in two places meant a
/// slop marker tokenized into a term of its own on the index side. `~0` asked
/// the dictionary for a term `0`, no record held one, and the candidate set was
/// empty — so `MATCHES '"ada lovelace"~0'` answered nothing with an index and
/// correctly with none.
pub(crate) fn asked_terms(analyzer: &Analyzer, query: &str) -> Vec<String> {
    match phrase_of(query) {
        Some((inner, _)) => analyzer.terms(inner),
        None => analyzer.terms(query),
    }
}
