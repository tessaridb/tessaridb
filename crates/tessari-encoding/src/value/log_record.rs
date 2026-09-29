//! A log record: the mutations one commit made, with its epoch and order.

use super::{
    EPOCH_LEN, FLAG_EPOCH, FLAG_ORDER, FLAG_SHARDS, HEADER_LEN, ORDER_LEN, StampedValue,
    StoreValue, split_header, with_header,
};
use crate::error::{Error, Result};
use crate::order::{KeyReader, KeyWriter};
use tessari_kv::Value;
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, Sequence, ShardId, TableId};

/// One mutation inside a log record: which record, and what it becomes.
///
/// It carries the record's **address**, not its encoded key. An encoded key has
/// the version baked into it, so a record whose embedded version disagreed with
/// its own log sequence would create a second ordering authority — the thing
/// ADR-0001 exists to prevent. Carrying the address and deriving the version
/// from the log entry's own sequence makes that disagreement unrepresentable
/// rather than merely forbidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
    /// Which shard of a split table the record falls in, decided by the writer
    /// at commit; `None` for a table that is not split.
    pub shard: Option<ShardId>,
    /// What the record becomes at this sequence, and what its writer had seen.
    pub value: StampedValue,
}

/// Everything one commit changed, as the log carries it.
///
/// Mutations are stored in address order. That is not tidiness: a deterministic
/// apply has to be fed a deterministic record, so the *encoder* is bound by the
/// same no-unordered-iteration rule the apply path is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogRecord {
    epoch: Epoch,
    /// Where this commit stands among every commit its writer made, in any log.
    order: Option<Sequence>,
    mutations: Vec<Mutation>,
}

impl LogRecord {
    /// Build a record from mutations that are already in address order.
    ///
    /// Sorting happens here rather than being assumed, because a caller that
    /// hands over an unordered collection would produce a record that replays to
    /// the same state but not to the same bytes — and byte-identical replay is
    /// the property being protected.
    #[must_use]
    pub fn new(mutations: Vec<Mutation>) -> Self {
        Self::at(Epoch::ZERO, mutations)
    }

    /// Build a record written under a named leadership.
    ///
    /// `new` is the same call at [`Epoch::ZERO`], which is the honest value for
    /// a store that has never elected anybody: this build allocates no epochs,
    /// so every record it writes belongs to the first and only leadership.
    #[must_use]
    pub fn at(epoch: Epoch, mut mutations: Vec<Mutation>) -> Self {
        mutations.sort_by(|left, right| {
            (left.namespace, left.database, left.table, &left.id).cmp(&(
                right.namespace,
                right.database,
                right.table,
                &right.id,
            ))
        });
        Self {
            epoch,
            order: None,
            mutations,
        }
    }

    /// State where its writer committed this record among all its commits
    /// (ADR-0084). Set by the commit that decides it, once per attempt.
    pub const fn set_order(&mut self, order: Sequence) {
        self.order = Some(order);
    }

    /// Where the writer committed this record among all its commits, or `None`
    /// for a record written before commits carried it.
    #[must_use]
    pub const fn order(&self) -> Option<Sequence> {
        self.order
    }

    /// The leadership that wrote this record.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The mutations this record applies, in address order.
    #[must_use]
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }

    /// Read the leadership out of encoded bytes without decoding the record.
    ///
    /// This is what the fixed offset was bought for. A store comparing the
    /// record it holds at a position against the one it is offered needs one
    /// number, and decoding the whole record to get it would put the cost of
    /// every mutation on a path that exists to be cheap.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated, carry an unsupported
    /// codec version, or set a reserved flag bit.
    pub fn epoch_in(bytes: &[u8]) -> Result<Epoch> {
        Ok(split_epoch(bytes)?.1)
    }

    /// Read the writer's commit order out of encoded bytes without decoding
    /// the mutations.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated, carry an unsupported
    /// codec version, or set a reserved flag bit.
    pub fn order_in(bytes: &[u8]) -> Result<Option<Sequence>> {
        let (flags, _, payload) = split_epoch(bytes)?;
        Ok(split_order(bytes, flags, payload)?.0)
    }
}

/// Split the optional epoch off an encoded log record.
///
/// One splitter rather than one in `decode` and another in `epoch_in`: two
/// readings of the same bytes is a thing that can disagree with itself, which is
/// the reason this record carries no mutation count either. Answers the flags as
/// well, because whether each mutation carries a shard is one of them.
fn split_epoch(bytes: &[u8]) -> Result<(u8, Epoch, &[u8])> {
    let (flags, payload) = split_header(bytes, FLAG_EPOCH | FLAG_SHARDS | FLAG_ORDER)?;
    if flags & FLAG_EPOCH == 0 {
        return Ok((flags, Epoch::ZERO, payload));
    }
    let raw: [u8; EPOCH_LEN] = payload
        .get(..EPOCH_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(EPOCH_LEN),
        })?;
    Ok((
        flags,
        Epoch::new(u64::from_be_bytes(raw)),
        payload.get(EPOCH_LEN..).unwrap_or_default(),
    ))
}

/// Split the optional commit order off what follows the epoch.
fn split_order<'a>(
    bytes: &[u8],
    flags: u8,
    payload: &'a [u8],
) -> Result<(Option<Sequence>, &'a [u8])> {
    if flags & FLAG_ORDER == 0 {
        return Ok((None, payload));
    }
    let raw: [u8; ORDER_LEN] = payload
        .get(..ORDER_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(ORDER_LEN),
        })?;
    Ok((
        Some(Sequence::new(u64::from_be_bytes(raw))),
        payload.get(ORDER_LEN..).unwrap_or_default(),
    ))
}

impl StoreValue for LogRecord {
    /// There is deliberately no mutation count in front of the mutations.
    ///
    /// Every mutation is self-delimiting — the record id is terminated and the
    /// value is length-prefixed — so a count would be a second statement of the
    /// same fact, which is a thing that can disagree with itself. It would also
    /// need a width, and a width needs a policy for what happens when a commit
    /// exceeds it. Neither question has to be answered if the field does not
    /// exist.
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::with_capacity(self.mutations.len().saturating_mul(32));
        // Decided over the whole record, because the field is per mutation or
        // not at all: a decoder has to know before the first mutation whether
        // each one carries it.
        let sharded = self
            .mutations
            .iter()
            .any(|mutation| mutation.shard.is_some());
        for mutation in &self.mutations {
            writer
                .put_u32(mutation.namespace.get())
                .put_u32(mutation.database.get())
                .put_u32(mutation.table.get());
            if sharded {
                // `0` is not a shard id, so it can stand for *this table is not
                // split* beside a mutation that is.
                writer.put_u32(mutation.shard.map_or(0, ShardId::get));
            }
            crate::record_id::put(&mut writer, &mutation.id);
            let encoded = mutation.value.encode();
            writer
                .put_u32(length_of(encoded.as_slice()))
                .put_fixed(encoded.as_slice());
        }
        let payload = writer.finish();
        // The epoch goes in front of the mutations rather than into them, so a
        // reader that only wants to know which leadership wrote this record
        // reads a fixed offset instead of walking every mutation in it.
        let (flags, epoch) = if self.epoch == Epoch::ZERO {
            (0, None)
        } else {
            (FLAG_EPOCH, Some(self.epoch.get().to_be_bytes()))
        };
        let flags = if sharded { flags | FLAG_SHARDS } else { flags };
        let flags = if self.order.is_some() {
            flags | FLAG_ORDER
        } else {
            flags
        };
        let mut buffer = with_header(
            flags,
            payload
                .len()
                .saturating_add(EPOCH_LEN)
                .saturating_add(ORDER_LEN),
        );
        if let Some(epoch) = epoch {
            buffer.extend_from_slice(&epoch);
        }
        // After the epoch, so the epoch keeps its fixed offset.
        if let Some(order) = self.order {
            buffer.extend_from_slice(&order.get().to_be_bytes());
        }
        buffer.extend_from_slice(&payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (flags, epoch, payload) = split_epoch(bytes)?;
        let (order, payload) = split_order(bytes, flags, payload)?;
        let sharded = flags & FLAG_SHARDS != 0;
        // The reader is the crate's bounds-checked byte cursor. The kind it is
        // built with only names the entity in a truncation error, and no kind
        // tag is consumed here — this is a value payload, not a key.
        let mut reader = KeyReader::new(crate::kind::KeyKind::LogEntry, payload);
        let mut mutations = Vec::new();
        while reader.remaining() > 0 {
            let namespace = NamespaceId::new(reader.take_u32()?);
            let database = DatabaseId::new(reader.take_u32()?);
            let table = TableId::new(reader.take_u32()?);
            let shard = if sharded {
                Some(reader.take_u32()?)
                    .filter(|raw| *raw != 0)
                    .map(ShardId::new)
            } else {
                None
            };
            let id = crate::record_id::take(&mut reader)?;
            let len = reader.take_u32()?;
            let encoded = reader.take_exact(usize::try_from(len).unwrap_or(usize::MAX))?;
            mutations.push(Mutation {
                namespace,
                database,
                table,
                id,
                shard,
                value: StampedValue::decode(&encoded)?,
            });
        }
        Ok(Self {
            epoch,
            order,
            mutations,
        })
    }
}

/// A byte length as it is written into a log record.
///
/// A value longer than a `u32` can express is not something this store can
/// produce — a single record that large would have failed on memory long before
/// reaching the codec — and saturating here keeps the encoder total rather than
/// making every caller of `encode` handle a case that cannot arise. The decoder
/// would reject the result as truncated, so the failure is loud either way.
pub(crate) fn length_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
}
