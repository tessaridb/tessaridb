//! `KEYS FROM space [RANGE a..b | PREFIX p] [AFTER k] [LIMIT n]` — a walk of a
//! space's keys that seeks to where it starts and stops where it ends.
//!
//! # A seek, never a scan of the space
//!
//! A key is a record identity, and identities are order-encoded, so every form
//! here names one contiguous stretch of the space. The walk opens at its first
//! key and stops at its last; keys outside it are never read. A prefix is the
//! stretch from the prefix itself up to — not including — the prefix with its
//! last character moved one code point on: UTF-8 keeps code-point order in byte
//! order, so every key that begins with the prefix lies strictly between the two
//! and no other key does.
//!
//! Expired keys are not listed: the walk resolves versions through the same
//! transaction read every other path takes.

use tessari_ql::{Expr, RangeExpr, TableRef};
use tessari_storage::{Transaction, Window};
use tessari_types::{RecordId, Value};

use crate::error::{Error, Result};
use crate::evaluate::key_bound;
use crate::outcome::Outcome;
use crate::session::Session;

/// What a `KEYS` statement asked for beyond the space.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk<'a> {
    /// `RANGE a..b`.
    pub(crate) range: Option<&'a RangeExpr>,
    /// `PREFIX p`.
    pub(crate) prefix: Option<&'a Expr>,
    /// `AFTER k`.
    pub(crate) after: Option<&'a Expr>,
    /// `LIMIT n`.
    pub(crate) limit: Option<u64>,
}

/// Where a walk starts, and where it stops with whether that key is inside;
/// `None` at either end is the end of the space.
struct Stretch {
    from: Option<RecordId>,
    to: Option<(RecordId, bool)>,
}

impl Session<'_> {
    /// The keys of a space, in key order.
    pub(crate) fn keys(
        &self,
        transaction: &mut Transaction<'_>,
        space: &TableRef,
        walk: Walk<'_>,
    ) -> Result<Outcome> {
        let (context, table) = self.resolve_table(transaction, space)?;
        let Stretch { from, to } = self.stretch(transaction, walk)?;
        let after = match walk.after {
            Some(after) => Some(key_bound(&self.evaluate(transaction, after)?, after.span)?),
            None => None,
        };
        let bound = walk.limit.map_or(usize::MAX, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        });
        if bound == 0 {
            return Ok(Outcome::Keys(Vec::new()));
        }
        let found = transaction.records_between(
            context.namespace,
            context.database,
            table,
            Window {
                from: from.as_ref(),
                to: to.as_ref().map(|(id, inclusive)| (id, *inclusive)),
            },
            after.as_ref(),
            bound,
        )?;
        Ok(Outcome::Keys(found.into_iter().map(|(id, _)| id).collect()))
    }

    /// The stretch of the space a walk names.
    fn stretch(&self, transaction: &mut Transaction<'_>, walk: Walk<'_>) -> Result<Stretch> {
        if let Some(range) = walk.range {
            let start = key_bound(&self.evaluate(transaction, &range.start)?, range.start.span)?;
            let end = key_bound(&self.evaluate(transaction, &range.end)?, range.end.span)?;
            return Ok(Stretch {
                from: Some(start),
                to: Some((end, range.inclusive)),
            });
        }
        let Some(prefix) = walk.prefix else {
            return Ok(Stretch {
                from: None,
                to: None,
            });
        };
        let Value::String(text) = self.evaluate(transaction, prefix)? else {
            return Err(Error::InvalidKeyBound { span: prefix.span });
        };
        if text.is_empty() {
            // Every text key begins with it, and so do none of the other kinds:
            // an empty prefix is not a stretch this walk can name exactly.
            return Err(Error::InvalidKeyBound { span: prefix.span });
        }
        let past = successor(&text).map(|past| (RecordId::Text(past), false));
        Ok(Stretch {
            from: Some(RecordId::Text(text)),
            to: past,
        })
    }
}

/// The least text greater than every text that begins with `prefix`, or `None`
/// when there is none (a prefix of nothing but the last code point).
fn successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        // The next scalar value, stepping over the surrogate gap that `char`
        // cannot hold.
        let next =
            (u32::from(last).saturating_add(1)..=u32::from(char::MAX)).find_map(char::from_u32);
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

#[cfg(test)]
mod tests;
