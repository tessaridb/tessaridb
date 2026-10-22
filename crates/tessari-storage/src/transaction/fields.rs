//! The instants some of a hash's fields stop being answered at, on a write
//! this transaction has buffered (ADR-0132).
//!
//! Beside `writes` for the reason `expiring` is: the buffered value is read in
//! a dozen places that have no use for them. A plain write clears them, as it
//! clears the record's own instant — the session's container writes restate
//! the ones that stand.

use tessari_encoding::{RecordValue, StampedValue};

use super::{RecordAddress, Transaction};
use crate::error::Result;

impl Transaction<'_> {
    /// Make the write buffered for `address` answer each of `fields` only
    /// until its instant, in milliseconds since the Unix epoch, replacing the
    /// instants it carried.
    ///
    /// Nothing happens when no present value is buffered there. A field whose
    /// instant is at or before this transaction's clock is removed from the
    /// write now, for the reason [`Transaction::expire_pending`] deletes: it
    /// would be gone the moment it was written. A hash left with no field is
    /// buffered as a deletion.
    pub fn expire_fields_pending(&mut self, address: &RecordAddress, fields: Vec<(String, u64)>) {
        let Some(RecordValue::Present(payload)) = self.writes.get(address) else {
            return;
        };
        let now = self.reading_at();
        let named =
            StampedValue::new(RecordValue::Present(payload.clone())).with_field_expiry(fields);
        let standing: Vec<(String, u64)> = named
            .field_expiry()
            .iter()
            .filter(|(_, at)| *at > now)
            .cloned()
            .collect();
        if standing.len() < named.field_expiry().len() {
            match named.into_visible_at(now) {
                RecordValue::Present(kept) => {
                    self.writes
                        .insert(address.clone(), RecordValue::Present(kept));
                }
                RecordValue::Tombstone => {
                    self.delete(address.clone());
                    return;
                }
            }
        }
        if standing.is_empty() {
            self.field_expiring.remove(address);
        } else {
            self.field_expiring.insert(address.clone(), standing);
        }
    }

    /// The instant each of some of a hash's fields stops being answered at, as
    /// this transaction sees it — its own buffered write first, then the
    /// stored version — sorted by field and only those still to come.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub async fn field_expiry(&mut self, address: &RecordAddress) -> Result<Vec<(String, u64)>> {
        if self.writes.contains_key(address) {
            return Ok(self
                .field_expiring
                .get(address)
                .cloned()
                .unwrap_or_default());
        }
        let now = self.reading_at();
        Ok(self
            .read_stamped_at(address)
            .await?
            .filter(|stamped| !stamped.is_expired_at(now))
            .map(|stamped| {
                stamped
                    .field_expiry()
                    .iter()
                    .filter(|(_, at)| *at > now)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}
