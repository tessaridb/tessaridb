//! Reading a file back from its chunks.

use super::{CHUNK, chunk_count, chunk_id, text_identity};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;
use tessari_encoding::decode_payload;
use tessari_ql::{RecordTarget, Span};
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{TableId, Value};

impl Session<'_> {
    /// `READ media:'/logo.png' [START n] [LIMIT n]` — a file's bytes, or part of
    /// them, or `NONE` if there is no such file.
    ///
    /// `START` and `LIMIT` mean here exactly what they mean over rows — skip
    /// this many, take this many — over bytes rather than records. A range that
    /// begins past the end of the file answers **empty**, which is the same
    /// answer a `START` past the last row already gives: a range that finds
    /// nothing is not a failure.
    ///
    /// Only the chunks the range touches are read, so asking for a kilobyte of a
    /// large file costs a kilobyte's worth of chunks rather than the file.
    pub(crate) fn read_file(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        start: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Outcome> {
        // A read has no ceiling to answer to: the bytes are already inside one.
        let (context, table, _) = self.bucket(transaction, target)?;
        let path = text_identity(target.id.fixed(target.span)?, target.span)?;
        let address = RecordAddress::new(
            context.namespace,
            context.database,
            table,
            target.id.fixed(target.span)?.clone(),
        );
        let Some(payload) = transaction.get(&address)? else {
            return Ok(Outcome::Value(Value::None));
        };
        let held = decode_payload(&payload)?;
        let chunks = self.chunk_table(transaction, &context, table, target.span)?;
        let total = chunk_count(&held);

        // The half-open byte range the caller asked for, and the chunks it
        // touches. `usize::MAX` for an absent limit is not a magic number doing
        // work: it is clamped by the file's own length below.
        let from = usize::try_from(start.unwrap_or(0)).unwrap_or(usize::MAX);
        let wanted = limit.map_or(usize::MAX, |held| {
            usize::try_from(held).unwrap_or(usize::MAX)
        });
        let until = from.saturating_add(wanted);
        let first = u32::try_from(from / CHUNK).unwrap_or(u32::MAX);
        let last = u32::try_from(until.saturating_sub(1) / CHUNK)
            .unwrap_or(u32::MAX)
            .min(total.saturating_sub(1));

        let mut bytes = Vec::new();
        if wanted == 0 || first >= total {
            return Ok(Outcome::Value(Value::Bytes(bytes)));
        }
        for ordinal in first..=last {
            let at = RecordAddress::new(
                context.namespace,
                context.database,
                chunks,
                chunk_id(path, ordinal),
            );
            let Some(part) = transaction.get(&at)? else {
                // A chunk the metadata promises and the store does not hold. It
                // cannot happen through this module — both are written in one
                // commit — so saying so is better than answering a short file.
                return Err(Error::FileIsIncomplete {
                    path: path.to_owned(),
                    ordinal,
                    span: target.span,
                });
            };
            match decode_payload(&part)? {
                Value::Bytes(part) => {
                    // Where this chunk sits in the file, intersected with what
                    // was asked for. The whole-file read is the case where the
                    // intersection is the chunk.
                    let base = usize::try_from(ordinal)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(CHUNK);
                    let begins = from.saturating_sub(base).min(part.len());
                    let ends = until.saturating_sub(base).min(part.len());
                    if let Some(slice) = part.get(begins..ends) {
                        bytes.extend_from_slice(slice);
                    }
                }
                _ => {
                    return Err(Error::FileIsIncomplete {
                        path: path.to_owned(),
                        ordinal,
                        span: target.span,
                    });
                }
            }
        }
        Ok(Outcome::Value(Value::Bytes(bytes)))
    }

    /// Every byte of a file, for a ranged write to splice into.
    pub(crate) fn whole_file(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        chunks: TableId,
        path: &str,
        payload: &[u8],
        span: Span,
    ) -> Result<Vec<u8>> {
        let held = decode_payload(payload)?;
        let mut bytes = Vec::new();
        for ordinal in 0..chunk_count(&held) {
            let at = RecordAddress::new(
                context.namespace,
                context.database,
                chunks,
                chunk_id(path, ordinal),
            );
            let Some(part) = transaction.get(&at)? else {
                return Err(Error::FileIsIncomplete {
                    path: path.to_owned(),
                    ordinal,
                    span,
                });
            };
            match decode_payload(&part)? {
                Value::Bytes(part) => bytes.extend_from_slice(&part),
                _ => {
                    return Err(Error::FileIsIncomplete {
                        path: path.to_owned(),
                        ordinal,
                        span,
                    });
                }
            }
        }
        Ok(bytes)
    }
}
