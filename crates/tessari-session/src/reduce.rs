//! A grouping read folded on the shards' leaders, so that only what each group
//! holds travels (ADR-0097 D2, D3).
//!
//! # What travels
//!
//! A [`Reduce`] names the fields the asker may read, the condition, the group
//! keys and the folds — each expression as [`tessari_ql::portable`] wrote it, so
//! no value becomes text. The leader answers one [`Partial`] per group a page
//! of records met: the key, the first identity, and what each fold holds so far.
//! The asker merges them in key order with its own records and answers the read
//! itself, so everything after the fold — the projection over the folded
//! values, `FILL`, `ORDER BY`, `LIMIT` — runs where it always ran.
//!
//! # Strict where the records path is lenient
//!
//! A gathered record is tested again by the asker, so the leader may keep
//! anything it is unsure of. A partial is not tested again: it **is** the
//! answer's input. So every doubt declines the page instead — a condition that
//! answers neither yes nor no, an evaluation that fails, a comparison across
//! two kinds (a note the asker would have shown), a total past what a float or
//! a decimal holds — and a declined page sends the whole read back to gathering
//! records, which answers it as before, refusal and note included. A float is
//! not a doubt: `sum`, `mean`, `variance` and `stddev` hold their float totals
//! exactly, so a merge answers the bits one walk does (ADR-0114).

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_ql::Aggregate;
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

use crate::accumulate::Accumulator;
use crate::condition::boolean;
use crate::error::Result;
use crate::evaluate::Scope;
use crate::noticed::Noticed;
use crate::redact::{Visible, seen};
use crate::session::Session;

/// An expression as [`tessari_ql::portable`] wrote it: the text and the values
/// its parameters stand for.
pub type Portable = (String, tessari_ql::Parameters);

/// What a node asks a shard's leader to fold its records into.
#[derive(Debug, Clone, PartialEq)]
pub struct Reduce {
    /// The fields the asker may read; `None` for all of them.
    pub visible: Visible,
    /// The condition a record must meet, when the read has one.
    pub condition: Option<Portable>,
    /// The `GROUP BY` expressions, in the order written.
    pub keys: Vec<Portable>,
    /// Every fold the read's projection holds, in the order a walk meets them.
    pub folds: Vec<Folded>,
}

/// One fold of a [`Reduce`].
#[derive(Debug, Clone, PartialEq)]
pub struct Folded {
    /// Which fold — one of the seven that merge exactly.
    pub fold: Aggregate,
    /// What it folds over; `None` for `count(*)`.
    pub over: Option<Portable>,
}

impl Folded {
    /// A fold named by its spelling, when it is one that merges exactly.
    #[must_use]
    pub fn named(spelling: &str, over: Option<Portable>) -> Option<Self> {
        let fold = [
            Aggregate::Count,
            Aggregate::Sum,
            Aggregate::Mean,
            Aggregate::Min,
            Aggregate::Max,
            Aggregate::Variance,
            Aggregate::Stddev,
        ]
        .into_iter()
        .find(|fold| fold.spelling() == spelling)?;
        Some(Self { fold, over })
    }
}

/// What one group held on one page of one shard.
#[derive(Debug, Clone, PartialEq)]
pub struct Partial {
    /// The group's key values.
    pub key: Vec<Value>,
    /// The first identity of the page that fell in the group.
    pub first: RecordId,
    /// One state per fold of the [`Reduce`], in its order.
    pub states: Vec<Value>,
}

/// What a leader answered a [`Reduce`] with.
#[derive(Debug, Clone, PartialEq)]
pub enum Reduced {
    /// Every group the records met, in key order.
    Partials(Vec<Partial>),
    /// The records could not be folded exactly here; gather them instead.
    Declined,
}

/// The groups `found` folds into under `reduce`, or `None` when they cannot be
/// folded exactly on this node.
///
/// # Errors
///
/// An expression that does not read back, or a store that cannot be read.
pub fn reducing(
    store: &Store,
    reduce: &Reduce,
    found: Vec<(RecordId, Vec<u8>)>,
) -> Result<Option<Vec<Partial>>> {
    let read = |(text, parameters): &Portable| tessari_ql::bound_condition(text, parameters);
    let condition = reduce.condition.as_ref().map(read).transpose()?;
    let keys = reduce
        .keys
        .iter()
        .map(read)
        .collect::<core::result::Result<Vec<_>, _>>()?;
    let mut folds = Vec::with_capacity(reduce.folds.len());
    for folded in &reduce.folds {
        folds.push((folded.fold, folded.over.as_ref().map(read).transpose()?));
    }
    let session = Session::new(store);
    let mut transaction = store.begin()?;
    let noticed = Noticed::default();
    let mut groups: BTreeMap<Vec<Value>, (RecordId, Vec<Accumulator>)> = BTreeMap::new();
    // Every doubt answers `None` rather than an error: the asker then gathers
    // the records and meets the same doubt in its own words.
    for (id, payload) in found {
        let Ok(record) = decode_payload(&payload) else {
            return Ok(None);
        };
        let record = seen(record, &reduce.visible);
        if let Some(condition) = &condition {
            let scope = Scope::of(&record).identified(&id).noticing(&noticed);
            let Ok(held) = session.evaluate_in(&mut transaction, condition, scope) else {
                return Ok(None);
            };
            let Ok(kept) = boolean(&held, condition.span) else {
                return Ok(None);
            };
            if !noticed.drained().is_empty() {
                return Ok(None);
            }
            if !kept {
                continue;
            }
        }
        // Evaluated as the asker's grouping evaluates them: over the record, and
        // with no identity beside it.
        let mut key = Vec::with_capacity(keys.len());
        for held in &keys {
            let Ok(value) = session.evaluate_in(&mut transaction, held, Scope::of(&record)) else {
                return Ok(None);
            };
            key.push(value);
        }
        let entry = groups.entry(key).or_insert_with(|| {
            // No span: a fold that fails here declines the page, so its
            // message is never shown — the asker's own fold names the place.
            let held = folds
                .iter()
                .map(|(fold, _)| Accumulator::for_aggregate(*fold, tessari_ql::Span::new(0, 0)))
                .collect();
            (id.clone(), held)
        });
        for ((_, over), accumulator) in folds.iter().zip(entry.1.iter_mut()) {
            let value = match over {
                None => Value::Bool(true),
                Some(over) => {
                    let Ok(value) = session.evaluate_in(&mut transaction, over, Scope::of(&record))
                    else {
                        return Ok(None);
                    };
                    value
                }
            };
            if accumulator.offer(&value).is_err() {
                return Ok(None);
            }
        }
    }
    transaction.rollback();
    let mut partials = Vec::with_capacity(groups.len());
    for (key, (first, held)) in groups {
        let Some(states) = held
            .iter()
            .map(Accumulator::state)
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        partials.push(Partial { key, first, states });
    }
    Ok(Some(partials))
}

/// What a grouping read asks the leaders to fold, when every part of it can be
/// folded there exactly — a whitelist, for `held_bound`'s reason: a clause added
/// later is a read gathered as records until somebody decides otherwise, never a
/// partial answer.
///
/// The visibility is left for the caller, which knows the session.
pub(crate) fn reduce_of(select: &tessari_ql::Select) -> Option<Reduce> {
    use tessari_ql::{ExprKind, Source};
    if !crate::evaluate::groups(select)
        || !select.fetch.is_empty()
        || select.split.is_some()
        || select.latest.is_some()
        || select.fusion.is_some()
        || select.after.is_some()
    {
        return None;
    }
    let condition = match &select.from {
        Source::Table(_) => None,
        Source::Where { condition, .. } => Some(tessari_ql::portable(condition)?),
        _ => return None,
    };
    let keys = select
        .group
        .iter()
        .map(tessari_ql::portable)
        .collect::<Option<Vec<_>>>()?;
    let mut folds = Vec::new();
    for held in crate::aggregate::occurrences(select.projection.written())
        .into_iter()
        .flatten()
    {
        let ExprKind::Fold { fold, over, at, .. } = &held.kind else {
            return None;
        };
        if at.is_some() {
            return None;
        }
        let over = match over {
            Some(over) => Some(tessari_ql::portable(over)?),
            None => None,
        };
        folds.push(Folded::named(fold.spelling(), over)?);
    }
    Some(Reduce {
        visible: None,
        condition,
        keys,
        folds,
    })
}

#[cfg(test)]
mod tests;
