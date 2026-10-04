//! A system table's rows whose identity begins with given bytes.
//!
//! The engine keeps some of its own bookkeeping under `RecordId::Bytes`
//! identities built from a fixed prefix — a view's membership map, a rollup's
//! exact sums — so "every row of this view" or "every row of this rollup" is a
//! walk over one span of identities. Byte identities sort as their bytes, so
//! the span runs from the prefix to the first byte string after every one that
//! begins with it.

use tessari_types::{RecordId, TableId};

use super::{Transaction, Window};
use crate::catalog::system::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
use crate::error::Result;

/// The first byte string after every one that begins with `prefix`, or `None`
/// when there is none (a prefix of `0xff` bytes alone).
fn past(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut next = prefix.to_vec();
    while let Some(last) = next.pop() {
        if let Some(raised) = last.checked_add(1) {
            next.push(raised);
            return Some(next);
        }
    }
    None
}

impl Transaction<'_> {
    /// The rows of system table `table` whose byte identity begins with
    /// `prefix`, this transaction's own writes included.
    pub(crate) fn system_rows_prefixed(
        &self,
        table: TableId,
        prefix: &[u8],
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let from = RecordId::Bytes(prefix.to_vec());
        let to = past(prefix).map(RecordId::Bytes);
        self.records_between(
            SYSTEM_NAMESPACE,
            SYSTEM_DATABASE,
            table,
            Window {
                from: Some(&from),
                to: to.as_ref().map(|to| (to, false)),
            },
            None,
            usize::MAX,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::past;

    #[test]
    fn past_is_the_first_string_after_every_extension() {
        assert_eq!(past(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(past(&[1, 0xff]), Some(vec![2]));
        assert_eq!(past(&[0xff, 0xff]), None);
        let prefix = [2_u8, 0, 0, 0, 7];
        let upper = past(&prefix).unwrap();
        for tail in [&[][..], &[0][..], &[0xff, 0xff][..]] {
            let mut extended = prefix.to_vec();
            extended.extend_from_slice(tail);
            assert!(extended.as_slice() >= prefix.as_slice() && extended < upper);
        }
    }
}
