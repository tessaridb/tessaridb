use super::*;

/// The section of a `Gather` frame carrying a pushed condition.
pub(super) const SECTION_PUSHED: u8 = 1;

/// The section of a `Gather` frame carrying how many records are enough.
pub(super) const SECTION_ENOUGH: u8 = 2;

/// The section of a `Gather` frame carrying the folds to answer instead.
pub(super) const SECTION_REDUCE: u8 = 3;

/// The section of a `Gather` frame carrying the order to rank by (ADR-0102).
pub(super) const SECTION_ORDERED: u8 = 4;

/// The section of a `Gather` frame carrying the search index to count
/// (ADR-0103).
pub(super) const SECTION_COUNTING: u8 = 5;

/// The section of a `Gathered` frame carrying what the records folded into —
/// past the optional resume, whose own first byte is `1`.
pub(super) const SECTION_REDUCED: u8 = 3;

/// The section of a `Gathered` frame carrying what the page counted
/// (ADR-0103).
pub(super) const SECTION_COUNTED: u8 = 4;

/// A pushed condition: the visible fields, then the condition as an expression.
pub(super) fn put_pushed(into: &mut Vec<u8>, pushed: &tessari_session::Pushed) {
    put_visible(into, &pushed.visible);
    put_portable(into, &pushed.condition, &pushed.parameters);
}

pub(super) fn take_pushed(from: &[u8], at: usize) -> Result<(tessari_session::Pushed, usize)> {
    let (visible, at) = take_visible(from, at)?;
    let ((condition, parameters), at) = take_portable(from, at)?;
    Ok((
        tessari_session::Pushed {
            visible,
            condition,
            parameters,
        },
        at,
    ))
}

pub(super) fn next(at: usize) -> Result<usize> {
    at.checked_add(1).ok_or(Error::Malformed)
}

pub(super) fn put_optional(into: &mut Vec<u8>, id: Option<&RecordId>) {
    match id {
        Some(id) => {
            into.push(1);
            put_id(into, id);
        }
        None => into.push(0),
    }
}

pub(super) fn take_optional(from: &[u8], at: usize) -> Result<(Option<RecordId>, usize)> {
    match from.get(at) {
        Some(0) => Ok((None, next(at)?)),
        Some(1) => {
            let (id, at) = take_id(from, next(at)?)?;
            Ok((Some(id), at))
        }
        _ => Err(Error::Malformed),
    }
}

/// A record identity on the peer link: its kind, then its value.
///
/// Exhaustive over [`RecordId`], so an identity kind added later is a compile
/// error here rather than a value this link cannot carry.
pub(super) fn put_id(into: &mut Vec<u8>, id: &RecordId) {
    match id {
        RecordId::Int(value) => {
            into.push(1);
            into.extend_from_slice(&value.to_be_bytes());
        }
        RecordId::Text(value) => {
            into.push(2);
            frame::put_text(into, value);
        }
        RecordId::Uuid(value) => {
            into.push(3);
            into.extend_from_slice(value);
        }
        RecordId::Bytes(value) => {
            into.push(4);
            frame::put_bytes(into, value);
        }
    }
}

pub(super) fn take_id(from: &[u8], at: usize) -> Result<(RecordId, usize)> {
    let body = next(at)?;
    match from.get(at) {
        Some(1) => {
            let (value, end) = frame::take_u64(from, body)?;
            Ok((RecordId::Int(i64::from_be_bytes(value.to_be_bytes())), end))
        }
        Some(2) => {
            let (value, end) = frame::take_text(from, body)?;
            Ok((RecordId::Text(value), end))
        }
        Some(3) => {
            let end = body.checked_add(16).ok_or(Error::Malformed)?;
            let mut value = [0_u8; 16];
            value.copy_from_slice(from.get(body..end).ok_or(Error::Malformed)?);
            Ok((RecordId::Uuid(value), end))
        }
        Some(4) => {
            let (value, end) = frame::take_bytes(from, body)?;
            Ok((RecordId::Bytes(value), end))
        }
        _ => Err(Error::Malformed),
    }
}
