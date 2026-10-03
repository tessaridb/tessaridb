//! Where in a `FROM SEARCH` record the query was answered: the best window of
//! its `SNIPPET` fields, and the marks `search::highlight(field)` answers.
//!
//! Both answer **byte ranges** and never markup (ADR-0052, ADR-0100 D2): the
//! store chooses where, and the caller renders. Both mark the terms the query
//! **reached** — a synonym, an expansion of a prefix or a misspelling — in the
//! field that reached them, never the query string itself (search-engine
//! highlighting from the matched terms).

use std::collections::BTreeMap;

use tessari_constants::SEARCH_SNIPPET_TOKENS;
use tessari_types::{Path, Value};

use super::query::{Answering, Probe, Shape, Text};
use super::{Hit, range};

/// The best window among the hit's `SNIPPET` fields, as
/// `{ field, start, end }` — `none` when no such field holds a matched word.
///
/// A window of [`SEARCH_SNIPPET_TOKENS`] tokens is ranked by how many distinct
/// query words it holds, then by how many matches, then by coming first, so a
/// passage covering the whole query beats one repeating one word of it.
pub(crate) fn best(hit: &Hit<'_>, record: Option<&Value>) -> Value {
    let Some(record) = record else {
        return Value::None;
    };
    let mut chosen: Option<((usize, usize), String, usize, usize)> = None;
    for field in hit.member.fields.iter().filter(|field| field.snippet) {
        let Some(Value::String(text)) = field.path.resolve(record) else {
            continue;
        };
        let tokens = hit.analyzer.spans(text);
        let surfaces = surfaces_for(hit, text);
        let matched: Vec<Option<usize>> = tokens
            .iter()
            .enumerate()
            .map(|(at, token)| {
                let surface = surfaces.get(at).unwrap_or(&token.term);
                hit.query
                    .scored
                    .iter()
                    .position(|probe| field.answering.answers(probe, &token.term, surface))
            })
            .collect();
        let width = SEARCH_SNIPPET_TOKENS.min(tokens.len());
        for start in 0..=tokens.len().saturating_sub(width) {
            let Some(window) = matched.get(start..start.saturating_add(width)) else {
                continue;
            };
            let distinct = window
                .iter()
                .flatten()
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let count = window.iter().flatten().count();
            if count == 0 {
                continue;
            }
            let better = chosen
                .as_ref()
                .is_none_or(|(held, ..)| (distinct, count) > *held);
            if better {
                let (Some(first), Some(last)) = (
                    tokens.get(start),
                    tokens.get(start.saturating_add(width).saturating_sub(1)),
                ) else {
                    continue;
                };
                chosen = Some((
                    (distinct, count),
                    field.path.to_string(),
                    first.bytes.start,
                    last.bytes.end,
                ));
            }
        }
    }
    let Some((_, field, start, end)) = chosen else {
        return Value::None;
    };
    let Value::Object(mut marks) = range(start, end) else {
        return Value::None;
    };
    marks.insert("field".to_owned(), Value::from(field.as_str()));
    Value::Object(marks)
}

/// The marks `search::highlight(field)` answers inside a `FROM SEARCH`: every
/// token of `text` the query reached in that field, or every run of a phrase.
pub(crate) fn marks(hit: &Hit<'_>, path: &Path, text: &str) -> Value {
    let Some(field) = hit.member.fields.iter().find(|field| field.path == *path) else {
        return Value::Array(Vec::new());
    };
    let tokens = hit.analyzer.spans(text);
    let terms: Vec<String> = tokens.iter().map(|token| token.term.clone()).collect();
    let surfaces = surfaces_for(hit, text);
    let held = Text {
        terms: &terms,
        surfaces: &surfaces,
    };
    let marked: Vec<usize> = match &hit.query.shape {
        Shape::Phrase { words, slop } => runs(&field.answering, held, words, *slop),
        Shape::Boolean { required, .. } => held
            .tokens()
            .enumerate()
            .filter(|(_, (term, surface))| {
                required
                    .iter()
                    .flatten()
                    .any(|probe| field.answering.answers(probe, term, surface))
            })
            .map(|(at, _)| at)
            .collect(),
    };
    let mut ranges: BTreeMap<usize, usize> = BTreeMap::new();
    for at in marked {
        if let Some(token) = tokens.get(at) {
            ranges.insert(token.bytes.start, token.bytes.end);
        }
    }
    Value::Array(
        ranges
            .into_iter()
            .map(|(start, end)| range(start, end))
            .collect(),
    )
}

/// The ordinals of every non-overlapping run of a phrase, left to right.
fn runs(answering: &Answering, held: Text<'_>, words: &[Probe], slop: usize) -> Vec<usize> {
    let mut marked = Vec::new();
    let mut from = 0_usize;
    while let Some(terms) = held.terms.get(from..) {
        let rest = Text {
            terms,
            surfaces: held.surfaces.get(from..).unwrap_or_default(),
        };
        let Some(run) = answering.run_of(rest, words, slop) else {
            break;
        };
        let Some(last) = run.last().copied() else {
            break;
        };
        marked.extend(run.iter().map(|at| at.saturating_add(from)));
        from = from.saturating_add(last).saturating_add(1);
    }
    marked
}

/// The text's surfaces when the query asks a fuzzy word, and nothing otherwise
/// — the only probe that reads them (Q-867).
fn surfaces_for(hit: &Hit<'_>, text: &str) -> Vec<String> {
    if hit.query.fuzzy() {
        hit.analyzer.surfaces(text)
    } else {
        Vec::new()
    }
}
