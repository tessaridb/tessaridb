//! An ordered `LIMIT` a shard's leader ranks before it sends (ADR-0102): the
//! frame section that carries it, and the page the leader answers with.

use tessari_constants::GATHER_PAGE_RECORDS;
use tessari_storage::{Store, Transaction, Window};
use tessari_types::RecordId;

use super::folds::{put_portable, put_visible, take_portable, take_visible};
use super::{Gather, Page, next};
use crate::error::{Error, Result};
use crate::frame;

/// The visible fields, each key as a portable expression and its direction,
/// then how many records the asker can take.
pub(super) fn put_ordered(into: &mut Vec<u8>, ordered: &tessari_session::Ordered) {
    put_visible(into, &ordered.visible);
    frame::put_u32(into, u32::try_from(ordered.keys.len()).unwrap_or(u32::MAX));
    for key in &ordered.keys {
        put_portable(into, &key.key, &key.parameters);
        into.push(u8::from(key.descending));
    }
    frame::put_u64(into, ordered.most);
}

pub(super) fn take_ordered(from: &[u8], at: usize) -> Result<(tessari_session::Ordered, usize)> {
    let (visible, at) = take_visible(from, at)?;
    let (count, mut at) = frame::take_u32(from, at)?;
    let mut keys = Vec::new();
    for _ in 0..count {
        let ((key, parameters), next_at) = take_portable(from, at)?;
        // Two meanings and no third: anything else is a frame read wrongly.
        let descending = match from.get(next_at) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(Error::Malformed),
        };
        keys.push(tessari_session::OrderKey {
            key,
            parameters,
            descending,
        });
        at = next(next_at)?;
    }
    let (most, at) = frame::take_u64(from, at)?;
    Ok((
        tessari_session::Ordered {
            visible,
            keys,
            most,
        },
        at,
    ))
}

/// The page `asked` gets from a shard whose first `most` records `ordered`
/// ranks: the whole window read a page at a time and narrowed by the pushed
/// condition, ranked, and sent in identity order past `asked.after`, cut by
/// `budget` like any page.
///
/// Ranked again for every page asked: a page is a separate request, and the
/// ranked set is at most `most` records, which is what the asker can take.
pub(super) fn ranked_page(
    store: &Store,
    transaction: &mut Transaction<'_>,
    asked: &Gather,
    ordered: &tessari_session::Ordered,
    window: Window<'_>,
    budget: usize,
) -> Result<Page> {
    let mut after: Option<RecordId> = None;
    let mut done = false;
    let ranked = tessari_session::leading(store, ordered, || {
        if done {
            return Ok(None);
        }
        let page = transaction.records_between(
            asked.namespace,
            asked.database,
            asked.table,
            window,
            after.as_ref(),
            GATHER_PAGE_RECORDS,
        )?;
        done = page.len() < GATHER_PAGE_RECORDS;
        after = page.last().map(|(id, _)| id.clone());
        Ok(Some(match &asked.pushed {
            Some(pushed) => tessari_session::keeping(store, pushed, page)?,
            None => page,
        }))
    })
    .map_err(|why| Error::Refused {
        message: why.to_string(),
    })?;
    let mut more = false;
    let mut records = Vec::new();
    let mut bytes = 0_usize;
    for (id, record) in ranked
        .into_iter()
        .filter(|(id, _)| asked.after.as_ref().is_none_or(|sent| id > sent))
    {
        // At least one record per page, whatever its size, as for any page.
        if !records.is_empty() && bytes.saturating_add(record.len()) > budget {
            more = true;
            break;
        }
        bytes = bytes.saturating_add(record.len());
        records.push((id, record));
    }
    Ok(Page {
        records,
        more,
        resume: None,
        reduced: None,
        counted: None,
    })
}
