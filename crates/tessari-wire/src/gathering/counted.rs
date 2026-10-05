//! A shard's search figures, counted by its leader for a partial holder's
//! score (ADR-0103): the frame sections that carry the question and the
//! answer, and the page the leader answers with.

use tessari_storage::{Catalog, SearchCounts, Transaction, Window};
use tessari_types::{IndexId, RecordId};

use super::{Gather, Page};
use crate::error::{Error, Result};
use crate::frame;

/// The search index, then each term asked.
pub(super) fn put_counting(into: &mut Vec<u8>, counting: &tessari_session::Counting) {
    frame::put_u32(into, counting.index.get());
    put_count(into, counting.terms.len());
    for term in &counting.terms {
        frame::put_text(into, term);
    }
}

pub(super) fn take_counting(from: &[u8], at: usize) -> Result<(tessari_session::Counting, usize)> {
    let (index, at) = frame::take_u32(from, at)?;
    let (count, mut at) = frame::take_u32(from, at)?;
    // Grown as read rather than sized by the peer's count, so a count the body
    // cannot hold costs a refusal and not an allocation.
    let mut terms = Vec::new();
    for _ in 0..count {
        let (term, next_at) = frame::take_text(from, at)?;
        terms.push(term);
        at = next_at;
    }
    Ok((
        tessari_session::Counting {
            index: IndexId::new(index),
            terms,
        },
        at,
    ))
}

/// Documents, tokens, then how many documents hold each asked term.
pub(super) fn put_counted(into: &mut Vec<u8>, counted: &SearchCounts) {
    frame::put_u64(into, counted.documents);
    frame::put_u64(into, counted.tokens);
    put_count(into, counted.holding.len());
    for held in &counted.holding {
        frame::put_u64(into, *held);
    }
}

pub(super) fn take_counted(from: &[u8], at: usize) -> Result<(SearchCounts, usize)> {
    let (documents, at) = frame::take_u64(from, at)?;
    let (tokens, at) = frame::take_u64(from, at)?;
    let (count, mut at) = frame::take_u32(from, at)?;
    let mut holding = Vec::new();
    for _ in 0..count {
        let (held, next_at) = frame::take_u64(from, at)?;
        holding.push(held);
        at = next_at;
    }
    Ok((
        SearchCounts {
            documents,
            tokens,
            holding,
        },
        at,
    ))
}

fn put_count(into: &mut Vec<u8>, count: usize) {
    frame::put_u32(into, u32::try_from(count).unwrap_or(u32::MAX));
}

/// The figures one page of `asked`'s window holds for the search index named
/// by `counting`, resuming past the last record read when more follow.
pub(super) fn counted_page(
    transaction: &mut Transaction<'_>,
    asked: &Gather,
    counting: &tessari_session::Counting,
    window: Window<'_>,
    page_records: usize,
) -> Result<Page> {
    let refused = |why: tessari_storage::Error| Error::Refused {
        message: why.to_string(),
        class: None,
    };
    let index = Catalog::new(transaction)
        .indexes_on(asked.table)
        .map_err(refused)?
        .into_iter()
        .find(|index| index.id == counting.index && index.search)
        .ok_or_else(|| Error::Refused {
            message: "the node asked has no such search index on this table".to_owned(),
            class: None,
        })?;
    let found = transaction
        .records_between(
            asked.namespace,
            asked.database,
            asked.table,
            window,
            asked.after.as_ref(),
            page_records,
        )
        .map_err(refused)?;
    let more = found.len() == page_records;
    let read_to: Option<RecordId> = found.last().map(|(id, _)| id.clone());
    let counted = transaction
        .search_counts(&index, &found, &counting.terms)
        .map_err(refused)?;
    Ok(Page {
        records: Vec::new(),
        more,
        resume: if more { read_to } else { None },
        reduced: None,
        counted: Some(counted),
    })
}
