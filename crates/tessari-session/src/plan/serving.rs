use std::collections::BTreeMap;

use tessari_storage::IndexDefinition;
use tessari_types::{Path, Value};

/// Every declared index whose **leading** field is this path, of the kind that
/// can answer the question asking.
///
/// An index serves a condition on its first field, whether or not it has others:
/// the key encoding puts that field first and byte order is value order, so the
/// entries for one value of it are contiguous. Only the first — the entries for
/// one value of a *later* field are scattered across every value of the ones
/// before it, so an index offered for that would be answering about the wrong
/// column.
///
/// **Every** such index, not the first one declared, and that is the wave's
/// restructuring in one line. Taking the first meant an index was never asked
/// what it could serve; it was handed a conjunct. So a composite `(last, first)`
/// was invisible whenever a plain `last` happened to be declared before it, and
/// a field carrying both a search index and an ordered one could be served by
/// neither — the search index won the search and then failed the equality guard.
///
/// A `Vec` rather than an iterator because the list is a handful of definitions
/// and a borrowing iterator here would tie the caller's hands for nothing.
pub(super) fn serving<'a>(
    declared: &'a [IndexDefinition],
    path: &Path,
    search: bool,
) -> Vec<&'a IndexDefinition> {
    declared
        .iter()
        .filter(|index| {
            index.fields.first() == Some(path)
                && if search {
                    index.search
                } else {
                    // Not `!index.search`: a vector or spatial index is not a
                    // search index and is not an ordered one either, and asking
                    // the negative admitted both. See `IndexDefinition::is_ordered`.
                    index.is_ordered()
                }
        })
        .collect()
}

/// Every declared index that can bound a range on this path, with the leading
/// run of values the condition fixes before it.
///
/// An index qualifies when `path` is one of its fields **and** every field
/// before it is fixed to an exact value by the condition. That is what makes the
/// entries the range walks contiguous: the key puts the fixed values first, so
/// fixing all of them names one run, and inside that run the entries are ordered
/// by the very field being bounded.
///
/// Position zero is the ordinary case and yields an empty run — a range on the
/// index's own leading field, which is every range this store served before.
/// Position `p > 0` with any of `0..p` unfixed does **not** qualify: the tags in
/// range would be scattered across every value of the fields before them, and
/// finding them means visiting each run's slice in turn. That is a different
/// traversal and it is deliberately not built here.
///
/// Only an ordered index qualifies at all. A search, vector or spatial index
/// holds terms, a graph or cells rather than an order over the value, so a range
/// over one would not be a scan wearing an index's name — it would be a lookup
/// in a keyspace that is not keyed by the value, answering with fewer rows.
pub(super) fn ranged<'a>(
    declared: &'a [IndexDefinition],
    path: &Path,
    fixed: &BTreeMap<&Path, &Value>,
) -> Vec<(&'a IndexDefinition, Vec<Value>)> {
    let mut found = Vec::new();
    for index in declared {
        if !index.is_ordered() {
            continue;
        }
        let Some(at) = index.fields.iter().position(|field| field == path) else {
            continue;
        };
        let mut run = Vec::with_capacity(at);
        for field in index.fields.iter().take(at) {
            let Some(held) = fixed.get(field) else {
                break;
            };
            run.push((*held).clone());
        }
        if run.len() == at {
            found.push((index, run));
        }
    }
    found
}

/// Every declared spatial index on this path.
///
/// Asked **positively**, like [`IndexDefinition::is_ordered`] and for the same
/// reason: a guard over index kinds that names the ones it skips admits every
/// kind added after it, and the symptom is a read answering with fewer rows and
/// no error at all. Asking `index.spatial` cannot go wrong that way — a seventh
/// kind is excluded here by default, which is slow and correct.
///
/// The leading field only, as everywhere else. A spatial index keys by the cells
/// covering one geometry, so a route that is not the one it was built on is a
/// question about a different column.
pub(super) fn spatial<'a>(
    declared: &'a [IndexDefinition],
    path: &Path,
) -> Vec<&'a IndexDefinition> {
    declared
        .iter()
        .filter(|index| index.spatial && index.fields.first() == Some(path))
        .collect()
}

/// The leading run of this index's fields the condition fixes to a value.
///
/// It stops at the first field nothing fixes, so the result is always a genuine
/// prefix of the index — which is what `Transaction::records_by_index` requires
/// and what makes "complete" mean the same thing on both sides.
pub(super) fn gathered(index: &IndexDefinition, fixed: &BTreeMap<&Path, &Value>) -> Vec<Value> {
    let mut values = Vec::new();
    for field in &index.fields {
        let Some(held) = fixed.get(field) else {
            break;
        };
        values.push((*held).clone());
    }
    values
}
