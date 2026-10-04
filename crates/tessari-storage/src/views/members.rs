//! Which group of a grouped materialized view each source record is in
//! (ADR-0109 D2, Q-908).
//!
//! The change feed names a record that changed and not what it held before, so
//! nothing in a change says which group the record **left**. This map does:
//! one system row per member, `record → group`, read before the record's new
//! group is computed. Its twin, `group → record`, is what lets a batch read one
//! group's members without reading the table.
//!
//! A group is held as the bytes of its key's order-preserving encoding, which is
//! also the identity of the view row that answers for it. Both rows are written
//! in the maintaining transaction beside the view's rows, so the map, the rows
//! and the view's state never disagree; they live in the system tenancy, so the
//! feed a maintainer reads never carries them, and they travel in the log like
//! the view's state does.
//!
//! # The keys
//!
//! - `0x01 · view · record` → the group's bytes
//! - `0x02 · view · group (escaped, terminated) · record` → the record's bytes
//!
//! The group is escaped and terminated, so one group's rows are exactly the
//! rows whose identity begins with its prefix.

use tessari_encoding::{
    KeyWriter, decode_payload, decode_record_id, encode_payload, encode_record_id,
};
use tessari_types::{RecordId, TableId, Value};

use crate::catalog::system::{self, VIEW_MEMBERS};
use crate::error::{Error, Result};
use crate::transaction::Transaction;

const RECORD_TO_GROUP: u8 = 0x01;
const GROUP_TO_RECORD: u8 = 0x02;

fn prefix(direction: u8, view: TableId) -> KeyWriter {
    let mut writer = KeyWriter::new();
    writer.put_u8(direction).put_u32(view.get());
    writer
}

fn group_of_id(view: TableId, record: &RecordId) -> RecordId {
    let mut key = prefix(RECORD_TO_GROUP, view).finish();
    key.extend_from_slice(&encode_record_id(record));
    RecordId::Bytes(key)
}

fn group_prefix(view: TableId, group: &[u8]) -> Vec<u8> {
    let mut writer = prefix(GROUP_TO_RECORD, view);
    writer.put_variable(group);
    writer.finish()
}

fn member_id(view: TableId, group: &[u8], record: &RecordId) -> RecordId {
    let mut key = group_prefix(view, group);
    key.extend_from_slice(&encode_record_id(record));
    RecordId::Bytes(key)
}

fn bytes_of(payload: &[u8], field: &'static str) -> Result<Vec<u8>> {
    match decode_payload(payload)? {
        Value::Bytes(bytes) => Ok(bytes),
        other => Err(Error::CatalogMalformed {
            entity: "view membership",
            field,
            found: other.type_name(),
        }),
    }
}

impl Transaction<'_> {
    /// The group of `view` that `record` was last put in, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or the row is malformed.
    pub fn view_group_of(&self, view: TableId, record: &RecordId) -> Result<Option<Vec<u8>>> {
        let address = system::address(VIEW_MEMBERS, group_of_id(view, record));
        self.get(&address)?
            .map(|payload| bytes_of(&payload, "group"))
            .transpose()
    }

    /// Move `record` from the group it was in, `left`, to `joined` — `None`
    /// on either side being no group.
    pub fn move_view_member(
        &mut self,
        view: TableId,
        record: &RecordId,
        left: Option<&[u8]>,
        joined: Option<&[u8]>,
    ) {
        if let Some(group) = left {
            self.delete(system::address(
                VIEW_MEMBERS,
                member_id(view, group, record),
            ));
        }
        let forward = system::address(VIEW_MEMBERS, group_of_id(view, record));
        match joined {
            Some(group) => {
                self.put(
                    forward,
                    encode_payload(&Value::Bytes(group.to_vec())).into_bytes(),
                );
                self.put(
                    system::address(VIEW_MEMBERS, member_id(view, group, record)),
                    encode_payload(&Value::Bytes(encode_record_id(record))).into_bytes(),
                );
            }
            None => self.delete(forward),
        }
    }

    /// Every record in one group of `view`, in identity order.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or a row is malformed.
    pub fn view_group_members(&self, view: TableId, group: &[u8]) -> Result<Vec<RecordId>> {
        self.member_rows(&group_prefix(view, group))?
            .into_iter()
            .map(|(_, payload)| {
                decode_record_id(&bytes_of(&payload, "record")?).map_err(Error::from)
            })
            .collect()
    }

    /// Remove every membership row `view` has.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn forget_view_members(&mut self, view: TableId) -> Result<()> {
        for direction in [RECORD_TO_GROUP, GROUP_TO_RECORD] {
            for (id, _) in self.member_rows(&prefix(direction, view).finish())? {
                self.delete(system::address(VIEW_MEMBERS, id));
            }
        }
        Ok(())
    }

    /// The membership rows whose identity begins with `prefix`, this
    /// transaction's own writes included.
    fn member_rows(&self, prefix: &[u8]) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.system_rows_prefixed(VIEW_MEMBERS, prefix)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn one_groups_prefix_is_not_a_prefix_of_another_groups_rows() {
        let view = TableId::new(40);
        let short = group_prefix(view, b"ab");
        let RecordId::Bytes(longer) = member_id(view, b"abc", &RecordId::Int(1)) else {
            unreachable!()
        };
        assert!(!longer.starts_with(&short));
    }
}
