//! The folds a gather may carry instead of records, and the page of groups a
//! leader answers them with (ADR-0097 D2).

use tessari_storage::Store;
use tessari_types::RecordId;

use super::{Page, next, put_id, take_id};
use crate::error::{Error, Result};
use crate::frame;

/// One page of a shard folded into its groups (ADR-0097 D2): no records, the
/// groups, and where the next page begins.
///
/// A page whose groups would pass the byte budget declines rather than being
/// cut, because a group cut in two would be two partials the asker cannot tell
/// from two groups; the asker then gathers the records, which page by bytes.
pub(super) fn folded(
    store: &Store,
    reduce: &tessari_session::Reduce,
    (found, more): (Vec<(RecordId, Vec<u8>)>, bool),
    budget: usize,
) -> Result<Page> {
    let read_to = found.last().map(|(id, _)| id.clone());
    let declined = Page {
        records: Vec::new(),
        more: false,
        resume: None,
        reduced: Some(tessari_session::Reduced::Declined),
        counted: None,
    };
    let Some(partials) =
        tessari_session::reducing(store, reduce, found).map_err(|why| Error::Refused {
            message: why.to_string(),
            class: None,
        })?
    else {
        return Ok(declined);
    };
    let page = Page {
        records: Vec::new(),
        more,
        resume: if more { read_to } else { None },
        reduced: Some(tessari_session::Reduced::Partials(partials)),
        counted: None,
    };
    // Measured as it will be sent; a page is a handful of groups far more often
    // than it is near the budget, so the second encoding is the cheap side.
    if page.encode().len() > budget {
        return Ok(declined);
    }
    Ok(page)
}

/// The fields an asker may read: `0` for all of them, else `1` and each name.
pub(super) fn put_visible(into: &mut Vec<u8>, visible: &tessari_session::redact::Visible) {
    match visible {
        Some(fields) => {
            into.push(1);
            frame::put_u32(into, u32::try_from(fields.len()).unwrap_or(u32::MAX));
            for field in fields {
                frame::put_text(into, field);
            }
        }
        None => into.push(0),
    }
}

pub(super) fn take_visible(
    from: &[u8],
    at: usize,
) -> Result<(tessari_session::redact::Visible, usize)> {
    match from.get(at) {
        Some(0) => Ok((None, next(at)?)),
        Some(1) => {
            let (count, mut at) = frame::take_u32(from, next(at)?)?;
            let mut fields = std::collections::BTreeSet::new();
            for _ in 0..count {
                let (field, next_at) = frame::take_text(from, at)?;
                fields.insert(field);
                at = next_at;
            }
            Ok((Some(fields), at))
        }
        _ => Err(Error::Malformed),
    }
}

/// An expression's text, then each parameter as a name and a value in the
/// store's own codec.
pub(super) fn put_portable(
    into: &mut Vec<u8>,
    text: &str,
    parameters: &tessari_session::Parameters,
) {
    frame::put_text(into, text);
    frame::put_u32(into, u32::try_from(parameters.len()).unwrap_or(u32::MAX));
    for (name, value) in parameters {
        frame::put_text(into, name);
        frame::put_bytes(into, &tessari_encoding::encode_payload(value).into_bytes());
    }
}

pub(super) fn take_portable(from: &[u8], at: usize) -> Result<(tessari_session::Portable, usize)> {
    let (text, mut at) = frame::take_text(from, at)?;
    let (count, next_at) = frame::take_u32(from, at)?;
    at = next_at;
    let mut parameters = tessari_session::Parameters::new();
    for _ in 0..count {
        let (name, next_at) = frame::take_text(from, at)?;
        let (value, next_at) = frame::take_bytes(from, next_at)?;
        parameters.insert(name, take_value(&value)?);
        at = next_at;
    }
    Ok(((text, parameters), at))
}

fn take_value(bytes: &[u8]) -> Result<tessari_types::Value> {
    tessari_encoding::decode_payload(bytes).map_err(|_| Error::Malformed)
}

/// Folds to answer: the visible fields, the condition if any, the keys, then
/// each fold as its spelling and what it folds over if anything.
///
/// A counter fold adds the instant it orders by and one byte, `1` when the
/// read asks for its samples (ADR-0121 D3, D6). Only a counter fold reads that
/// byte, and only a leader of `0.32.0` or later is ever sent one, so what an
/// older leader is sent is byte for byte what it always was.
pub(super) fn put_reduce(into: &mut Vec<u8>, reduce: &tessari_session::Reduce) {
    put_visible(into, &reduce.visible);
    match &reduce.condition {
        Some((text, parameters)) => {
            into.push(1);
            put_portable(into, text, parameters);
        }
        None => into.push(0),
    }
    frame::put_u32(into, u32::try_from(reduce.keys.len()).unwrap_or(u32::MAX));
    for (text, parameters) in &reduce.keys {
        put_portable(into, text, parameters);
    }
    frame::put_u32(into, u32::try_from(reduce.folds.len()).unwrap_or(u32::MAX));
    for folded in &reduce.folds {
        frame::put_text(into, folded.fold.spelling());
        match &folded.over {
            Some((text, parameters)) => {
                into.push(1);
                put_portable(into, text, parameters);
            }
            None => into.push(0),
        }
        if let Some((text, parameters)) = &folded.at {
            put_portable(into, text, parameters);
            into.push(u8::from(reduce.samples));
        }
    }
}

pub(super) fn take_reduce(from: &[u8], at: usize) -> Result<(tessari_session::Reduce, usize)> {
    let (visible, at) = take_visible(from, at)?;
    let optional = |at: usize| -> Result<(Option<tessari_session::Portable>, usize)> {
        match from.get(at) {
            Some(0) => Ok((None, next(at)?)),
            Some(1) => {
                let (held, at) = take_portable(from, next(at)?)?;
                Ok((Some(held), at))
            }
            _ => Err(Error::Malformed),
        }
    };
    let (condition, at) = optional(at)?;
    let (count, mut at) = frame::take_u32(from, at)?;
    let mut keys = Vec::new();
    for _ in 0..count {
        let (key, next_at) = take_portable(from, at)?;
        keys.push(key);
        at = next_at;
    }
    let (count, next_at) = frame::take_u32(from, at)?;
    at = next_at;
    let mut folds = Vec::new();
    let mut samples = false;
    for _ in 0..count {
        let (spelling, next_at) = frame::take_text(from, at)?;
        let (over, mut next_at) = optional(next_at)?;
        let at_instant = if tessari_ql::Aggregate::parse(&spelling)
            .is_some_and(tessari_ql::Aggregate::takes_an_instant)
        {
            let (held, after) = take_portable(from, next_at)?;
            samples = match from.get(after) {
                Some(0) => samples,
                Some(1) => true,
                _ => return Err(Error::Malformed),
            };
            next_at = next(after)?;
            Some(held)
        } else {
            None
        };
        // A fold this build does not merge exactly is not a request it can
        // answer, and reading it as some other fold would answer another one.
        folds.push(
            tessari_session::Folded::named(&spelling, over, at_instant).ok_or(Error::Malformed)?,
        );
        at = next_at;
    }
    Ok((
        tessari_session::Reduce {
            visible,
            condition,
            keys,
            folds,
            samples,
        },
        at,
    ))
}

/// What a page folded into: `0` declined, else `1` and each group as its first
/// identity, its key and its states, each list in the store's own codec.
pub(super) fn put_reduced(into: &mut Vec<u8>, reduced: &tessari_session::Reduced) {
    let tessari_session::Reduced::Partials(partials) = reduced else {
        into.push(0);
        return;
    };
    into.push(1);
    frame::put_u32(into, u32::try_from(partials.len()).unwrap_or(u32::MAX));
    for partial in partials {
        put_id(into, &partial.first);
        for held in [&partial.key, &partial.states] {
            let list = tessari_types::Value::Array(held.clone());
            frame::put_bytes(into, &tessari_encoding::encode_payload(&list).into_bytes());
        }
    }
}

pub(super) fn take_reduced(from: &[u8], at: usize) -> Result<(tessari_session::Reduced, usize)> {
    match from.get(at) {
        Some(0) => Ok((tessari_session::Reduced::Declined, next(at)?)),
        Some(1) => {
            let (count, mut at) = frame::take_u32(from, next(at)?)?;
            let mut partials = Vec::new();
            for _ in 0..count {
                let (first, next_at) = take_id(from, at)?;
                let (key, next_at) = frame::take_bytes(from, next_at)?;
                let (states, next_at) = frame::take_bytes(from, next_at)?;
                let (tessari_types::Value::Array(key), tessari_types::Value::Array(states)) =
                    (take_value(&key)?, take_value(&states)?)
                else {
                    return Err(Error::Malformed);
                };
                partials.push(tessari_session::Partial { key, first, states });
                at = next_at;
            }
            Ok((tessari_session::Reduced::Partials(partials), at))
        }
        _ => Err(Error::Malformed),
    }
}
