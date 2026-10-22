//! A change made to a string's bytes between reading them and writing them
//! back: `APPEND`, `SETRANGE`, `INCRBYFLOAT`.

use crate::codec::Reply;
use crate::surface::values;

/// A change made to a string's bytes between reading and writing them.
#[derive(Debug, Clone)]
pub(crate) enum Change {
    Append(Vec<u8>),
    SetRange { offset: usize, bytes: Vec<u8> },
    IncrementFloat(f64),
}

/// What a change makes of the bytes held, or the reply refusing it.
pub(crate) fn changed(held: Option<&[u8]>, change: &Change) -> Result<Vec<u8>, Reply> {
    let held = held.unwrap_or_default();
    match change {
        Change::Append(more) => {
            let mut bytes = held.to_vec();
            bytes.extend_from_slice(more);
            Ok(bytes)
        }
        Change::SetRange { offset, bytes } => {
            let end = offset
                .checked_add(bytes.len())
                .filter(|end| *end <= tessari_constants::RESP_MAX_BULK_BYTES)
                .ok_or_else(|| Reply::error("ERR string exceeds maximum allowed size"))?;
            let mut changed = held.to_vec();
            if changed.len() < end {
                changed.resize(end, 0);
            }
            changed[*offset..end].copy_from_slice(bytes);
            Ok(changed)
        }
        Change::IncrementFloat(by) => {
            let base = if held.is_empty() {
                0.0
            } else {
                values::float(held).ok_or_else(|| Reply::error("ERR value is not a valid float"))?
            };
            let next = base + by;
            if !next.is_finite() {
                return Err(Reply::error("ERR increment would produce NaN or Infinity"));
            }
            Ok(values::float_text(next))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_is_made_to_the_held_bytes() {
        assert_eq!(
            changed(Some(b"Hello"), &Change::Append(b" World".to_vec())).unwrap(),
            b"Hello World"
        );
        assert_eq!(
            changed(
                None,
                &Change::SetRange {
                    offset: 3,
                    bytes: b"ab".to_vec()
                }
            )
            .unwrap(),
            b"\0\0\0ab"
        );
        assert_eq!(
            changed(Some(b"10.5"), &Change::IncrementFloat(0.1)).unwrap(),
            b"10.6"
        );
        assert!(changed(Some(b"abc"), &Change::IncrementFloat(1.0)).is_err());
        assert!(
            changed(
                None,
                &Change::SetRange {
                    offset: usize::MAX,
                    bytes: b"x".to_vec()
                }
            )
            .is_err()
        );
    }
}
