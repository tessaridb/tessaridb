//! What a record contributes to an index: values, cells, vectors and terms.

use crate::catalog::{Catalog, IndexDefinition};
use crate::error::Result;
use crate::graph;
use crate::transaction::Transaction;
use std::collections::{BTreeMap, BTreeSet};
use tessari_constants::SPATIAL_INDEX_CELLS_PER_RECORD;
use tessari_encoding::{IndexValues, Located};
use tessari_geo::{Bounds, Cell, Shape};
use tessari_types::{Analyzer, TableId, Value};

/// The analyzer each analysed field of a table declares, by field name.
pub(crate) fn analyzers_on(
    view: &mut Transaction<'_>,
    table: TableId,
) -> Result<BTreeMap<String, Analyzer>> {
    let declared = Catalog::new(view).fields_on(table)?;
    let wanted: BTreeMap<String, String> = declared
        .into_iter()
        .filter_map(|field| field.analyzer.map(|named| (field.name, named)))
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut resolved = BTreeMap::new();
    for definition in Catalog::new(view).analyzers()? {
        for (field, named) in &wanted {
            if *named == definition.name {
                resolved.insert(field.clone(), definition.analyzer.clone());
            }
        }
    }
    Ok(resolved)
}

/// The analyzer a search index reads its field's text with.
pub(crate) fn search_analyzer<'a>(
    definition: &IndexDefinition,
    analyzers: &'a BTreeMap<String, Analyzer>,
) -> Option<&'a Analyzer> {
    definition
        .fields
        .first()
        .and_then(|path| analyzers.get(path.root()))
}

/// What one record contributes to a search index: its postings, and its length.
///
/// Both come out of a **single** analyzer pass. They could each be computed on
/// their own, and then a change to the tokenizer would have to reach two places
/// to keep the postings and the statistics describing the same text — which is
/// the shape of drift that gets noticed as a ranking that is subtly wrong.
#[derive(Debug, Default)]
pub(crate) struct Analysed {
    /// One entry per **distinct** term, with the number of times it occurs.
    ///
    /// Still one posting per distinct term — a duplicate key would be written
    /// twice to say the same thing — but the count is no longer discarded on the
    /// way there. It is what a relevance score means by *how often*, and it can
    /// only be taken here, from the same analyzer pass that produced the terms.
    pub(crate) postings: Vec<(IndexValues, u32)>,
    /// Where each of those terms sits, entry for entry — the ordinals and byte
    /// ranges a `POSITIONS` / `OFFSETS` index stores, empty lists otherwise.
    pub(crate) located: Vec<Located>,
    /// How many tokens the text holds, **with** repeats — this is a length, and
    /// a length that collapsed repeats would not be one.
    pub(crate) tokens: u64,
    /// Each field's token count, for a search member; empty for a field index.
    pub(crate) fields: Vec<u64>,
    /// Each distinct `(surface, term)` pair whose term the stemmer changed —
    /// the raw companion a fuzzy word is measured against (Q-867). Empty for a
    /// chain without a stemmer, where every term is its own surface.
    pub(crate) surfaces: BTreeSet<(String, String)>,
}

impl Analysed {
    /// The record's length as a posting states it.
    ///
    /// Narrowed rather than cast: a document of more than four billion tokens
    /// saturates, and a saturated length makes a score slightly wrong for one
    /// absurd record where a wrapped one would make it wrong by an arbitrary
    /// amount for that record and correct-looking for every other.
    pub(crate) fn length(&self) -> u32 {
        u32::try_from(self.tokens).unwrap_or(u32::MAX)
    }
}

/// The box around one record's geometry, and the cells covering that box.
///
/// `None` when the field is absent or holds something that is not a geometry —
/// the same "not in this index at all" answer an ordered index gives for a
/// missing field, and the same answer a scan gives for the same record.
///
/// A geometry that will not lower to the grid gets the same answer, and cannot
/// arise for a stored record: positions are snapped at ingest and a geometry off
/// the sphere is refused there rather than here. Returning "not indexed" for it
/// keeps this function total without inventing a second refusal path for a case
/// the write path has already closed.
///
/// The cell count is bounded by [`SPATIAL_INDEX_CELLS_PER_RECORD`], and the
/// covering keeps a coarser cell rather than dropping a finer one when that
/// bound is reached — so exceeding the budget costs candidates to refine and
/// never rows.
pub(crate) fn covering_of(
    definition: &IndexDefinition,
    value: &Value,
) -> Option<(Bounds, Vec<Cell>)> {
    let path = definition.fields.first()?;
    let Value::Geometry(geometry) = path.resolve(value)? else {
        return None;
    };
    let bounds = Shape::of(geometry).ok()?.bounds()?;
    // The class each cell comes with is discarded, and only here: it says
    // whether the cell lies wholly inside the box it was produced for, which for
    // a *record's* own box tells a reader nothing. It is a query-side fact — the
    // reader's box is what decides whether a candidate may skip the predicate —
    // so keeping it in the entry would store an answer to a question nobody has
    // asked yet.
    let cells = tessari_geo::covering(bounds, SPATIAL_INDEX_CELLS_PER_RECORD)
        .into_iter()
        .map(|(cell, _)| cell)
        .collect();
    Some((bounds, cells))
}

/// The vector one record contributes to a vector index.
///
/// `None` when the field is absent, holds something that is not an array of
/// numbers, or holds an empty one — the same "not in this index at all" answer
/// an ordered index gives for a missing field, and the same reading the
/// language's own distance functions do.
pub(crate) fn projected_vector(definition: &IndexDefinition, value: &Value) -> Option<Vec<f64>> {
    let path = definition.fields.first()?;
    graph::vector_of(path.resolve(value)?)
}

/// The terms one record contributes to a search index.
///
/// Empty when the field declares no analyzer, holds no text, or the record does
/// not have it — the same "not in this index at all" answer an ordered index
/// gives, and the same answer a scan gives for the same record.
pub(crate) fn terms_of(
    definition: &IndexDefinition,
    analyzer: Option<&Analyzer>,
    value: &Value,
) -> Analysed {
    let (Some(analyzer), Some(path)) = (analyzer, definition.fields.first()) else {
        return Analysed::default();
    };
    let Some(Value::String(text)) = path.resolve(value) else {
        return Analysed::default();
    };
    if definition.costs.positions || definition.costs.offsets {
        return located_terms_of(definition, analyzer, text);
    }
    let mut terms: Vec<String> = analyzer.terms(text);
    let mut surfaces = BTreeSet::new();
    paired(&mut surfaces, analyzer, text, &terms);
    let tokens = u64::try_from(terms.len()).unwrap_or(u64::MAX);
    terms.sort_unstable();
    // Sorting puts equal terms next to each other, so a run *is* the count. This
    // replaces a `dedup()` that threw the run length away — the same pass, one
    // number further.
    let postings: Vec<(IndexValues, u32)> = terms
        .chunk_by(|held, next| held == next)
        .filter_map(|run| {
            let term = run.first()?;
            let frequency = u32::try_from(run.len()).unwrap_or(u32::MAX);
            Some((IndexValues::of(&[Value::from(term.as_str())]), frequency))
        })
        .collect();
    Analysed {
        located: vec![Located::default(); postings.len()],
        postings,
        tokens,
        fields: Vec::new(),
        surfaces,
    }
}

/// Add the `(surface, term)` pairs of `text` whose term the stemmer changed.
///
/// `terms` is the text's analysis in token order, so the two lists line up
/// one for one — the property [`Analyzer::surfaces`] states.
fn paired(
    into: &mut BTreeSet<(String, String)>,
    analyzer: &Analyzer,
    text: &str,
    terms: &[String],
) {
    if !analyzer.stems() {
        return;
    }
    for (surface, term) in analyzer.surfaces(text).into_iter().zip(terms) {
        if surface != *term {
            into.insert((surface, term.clone()));
        }
    }
}

/// [`terms_of`] for an index keeping positions or offsets: the same terms and
/// counts, from the analyzer's spans so each occurrence keeps its ordinal and
/// its bytes (ADR-0100 D4).
///
/// The spans are the tokens [`Analyzer::terms`] produces, in the same order and
/// with the same empty tokens dropped, so an ordinal here is an index into the
/// list the scan's phrase test walks — which is what makes a phrase decided from
/// stored ordinals the same answer.
fn located_terms_of(definition: &IndexDefinition, analyzer: &Analyzer, text: &str) -> Analysed {
    let narrow = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    let mut held: Vec<(String, u32, (u32, u32))> = analyzer
        .spans(text)
        .into_iter()
        .enumerate()
        .map(|(ordinal, token)| {
            let bytes = (narrow(token.bytes.start), narrow(token.bytes.end));
            (token.term, narrow(ordinal), bytes)
        })
        .collect();
    let mut surfaces = BTreeSet::new();
    let in_order: Vec<String> = held.iter().map(|(term, _, _)| term.clone()).collect();
    paired(&mut surfaces, analyzer, text, &in_order);
    let tokens = u64::try_from(held.len()).unwrap_or(u64::MAX);
    // By term, then by ordinal, so each run lists its occurrences in order.
    held.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let mut postings = Vec::new();
    let mut located = Vec::new();
    for run in held.chunk_by(|left, right| left.0 == right.0) {
        let Some((term, _, _)) = run.first() else {
            continue;
        };
        postings.push((
            IndexValues::of(&[Value::from(term.as_str())]),
            narrow(run.len()),
        ));
        located.push(Located {
            positions: if definition.costs.positions {
                run.iter().map(|(_, ordinal, _)| *ordinal).collect()
            } else {
                Vec::new()
            },
            offsets: if definition.costs.offsets {
                run.iter().map(|(_, _, bytes)| *bytes).collect()
            } else {
                Vec::new()
            },
            fields: Vec::new(),
        });
    }
    Analysed {
        postings,
        located,
        tokens,
        fields: Vec::new(),
        surfaces,
    }
}

/// The entries one record contributes to an index — none, one, or several.
///
/// An empty answer means "not in this index" rather than "indexed under
/// nothing": a record that is not an object has nothing to project, and a record
/// where one of the indexed routes reaches nothing has no value to place.
///
/// A route reaching nothing covers more ground than a missing field did: a
/// missing intermediate, an object addressed by position, an array addressed by
/// name. All of them are the same answer, and it is the same answer a missing
/// top-level field has always given, which is what lets documents of differing
/// shapes share a table without the index having an opinion about it.
///
/// # Several, and why the general shape is the simpler one to be right about
///
/// A route holding `[*]` reaches several values, and the record contributes one
/// entry per value — which is what a multikey index *is*. Written as a product
/// over the routes, so a route with no `[*]` contributes exactly the one value it
/// contributes today and the composite case needs no second rule. `DEFINE INDEX`
/// admits at most one multi-valued route, so the product never actually
/// multiplies; the code does not need to know that, and a version that did would
/// be longer and would have a branch nothing exercises.
///
/// Entries are **deduplicated**, so `tags: ['dup', 'dup']` is one entry rather
/// than an entry written twice. The batch would make that idempotent anyway; the
/// point is that the remove side runs this same function, and two sides that
/// agree by construction cannot leave an orphan behind.
pub(crate) fn project(definition: &IndexDefinition, value: &Value) -> Vec<IndexValues> {
    let mut rows: Vec<Vec<Value>> = vec![Vec::with_capacity(definition.fields.len())];
    for path in &definition.fields {
        let reached: Vec<&Value> = if path.is_several() {
            path.reach(value)
        } else {
            match path.resolve(value) {
                Some(Value::None) | None => return Vec::new(),
                Some(found) => vec![found],
            }
        };
        if reached.is_empty() {
            return Vec::new();
        }
        rows = rows
            .iter()
            .flat_map(|held| {
                reached.iter().map(|found| {
                    let mut next = held.clone();
                    next.push((*found).clone());
                    next
                })
            })
            .collect();
    }
    rows.iter()
        .map(|held| IndexValues::of(held))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The analyzer an index reads its text with: a member's is the search's,
/// named; a field index's is its field's declaration.
pub(crate) fn analyzer_for<'a>(
    definition: &IndexDefinition,
    declared: &'a BTreeMap<String, Analyzer>,
    named: &'a BTreeMap<String, Analyzer>,
) -> Option<&'a Analyzer> {
    match &definition.engine {
        Some(engine) => named.get(&engine.analyzer),
        None => search_analyzer(definition, declared),
    }
}

/// Every analyzer the store declares, by name — what a search member resolves
/// its analyzer against.
pub(crate) fn analyzers_named(view: &mut Transaction<'_>) -> Result<BTreeMap<String, Analyzer>> {
    Ok(Catalog::new(view)
        .analyzers()?
        .into_iter()
        .map(|definition| (definition.name, definition.analyzer))
        .collect())
}

/// What one record posts to an index: [`terms_of`] for a field index, and for
/// a search member every field's terms together (ADR-0105).
///
/// A member's posting is the record's **total** over its fields — one
/// frequency and one length, the counted form every reader already knows — and
/// its per-field token counts travel beside it for the statistics, which are
/// what a BM25F field average is measured against.
pub(crate) fn analysed(
    definition: &IndexDefinition,
    analyzer: Option<&Analyzer>,
    value: &Value,
) -> Analysed {
    if definition.engine.is_none() {
        return terms_of(definition, analyzer, value);
    }
    let Some(analyzer) = analyzer else {
        return Analysed::default();
    };
    // Each token beside the ordinal of the field it came from, so a posting can
    // say how often its term occurs in each field (Q-870).
    let mut terms: Vec<(String, usize)> = Vec::new();
    let mut fields = Vec::with_capacity(definition.fields.len());
    let mut surfaces = BTreeSet::new();
    for (at, path) in definition.fields.iter().enumerate() {
        let held = match path.resolve(value) {
            Some(Value::String(text)) => {
                let held = analyzer.terms(text);
                paired(&mut surfaces, analyzer, text, &held);
                held
            }
            _ => Vec::new(),
        };
        fields.push(u64::try_from(held.len()).unwrap_or(u64::MAX));
        terms.extend(held.into_iter().map(|term| (term, at)));
    }
    let tokens = u64::try_from(terms.len()).unwrap_or(u64::MAX);
    if tokens == 0 {
        return Analysed::default();
    }
    terms.sort_unstable();
    let narrow = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
    let mut postings = Vec::new();
    let mut located = Vec::new();
    for run in terms.chunk_by(|held, next| held.0 == next.0) {
        let Some((term, _)) = run.first() else {
            continue;
        };
        let mut per_field = vec![0_u32; fields.len()];
        for (_, at) in run {
            if let Some(count) = per_field.get_mut(*at) {
                *count = count.saturating_add(1);
            }
        }
        postings.push((
            IndexValues::of(&[Value::from(term.as_str())]),
            u32::try_from(run.len()).unwrap_or(u32::MAX),
        ));
        located.push(Located {
            fields: per_field
                .into_iter()
                .zip(&fields)
                .map(|(frequency, length)| (frequency, narrow(*length)))
                .collect(),
            ..Located::default()
        });
    }
    Analysed {
        located,
        postings,
        tokens,
        fields,
        surfaces,
    }
}
