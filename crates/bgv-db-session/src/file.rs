//! Files: a bucket's metadata, and the chunks its bytes live in.
//!
//! # A file is a record, and so is a chunk
//!
//! ADR-0011. `media:'/logo.png'` is an ordinary record in an ordinary table, and
//! its payload is what the store knows about the file — how big it is, how many
//! chunks it took, when it was written. The bytes are records too, in a
//! companion table whose name carries a byte an identifier cannot hold, so no
//! statement can reach them.
//!
//! What that buys is everything already built. Snapshot isolation, deterministic
//! apply, version reclamation, the change feed, backup and restore, tenancy in
//! the key, per-table grants — none of them is extended by a line, and the
//! restore-and-compare test covers files the day they exist.
//!
//! # Why a chunk's identity is bytes
//!
//! `Bytes(path ++ ordinal:u32 big-endian)`. The path first so one file's chunks
//! are contiguous, the ordinal fixed-width and big-endian so they are in order —
//! record identities are order-encoded, so that ordering is the store's and not
//! a convention this module maintains.
//!
//! It also forces a file's own identity to be **text**: an integer identity and
//! the text of that integer would produce the same chunk key, and two files
//! sharing chunks is not a bug anybody would find twice.

use bgv_db_encoding::{decode_payload, encode_payload};
use bgv_db_ql::{RecordTarget, Span};
use bgv_db_storage::{Catalog, RecordAddress, Transaction};
use bgv_db_types::{RecordId, TableId, Value};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// How much of a file one record holds.
///
/// A megabyte: comfortably inside the wire protocol's 16 MiB frame and the
/// engine's write batch, and large enough that an ordinary document is one or
/// two records rather than hundreds.
const CHUNK: usize = 1024 * 1024;

/// What a file's metadata record holds.
const FIELD_SIZE: &str = "size";
const FIELD_CHUNKS: &str = "chunks";
const FIELD_UPDATED: &str = "updated";

impl Session<'_> {
    /// `PUT media:'/logo.png' = 0x…` — write a file, whole or in part.
    ///
    /// One commit either way, so a half-written file is not a state this store
    /// can be in: the chunks and the metadata that describes them land together
    /// or neither does. That is what makes a **ranged** write possible without a
    /// visibility rule — there is no moment between two commits for a reader to
    /// see, because there are not two commits. What remains genuinely absent is a
    /// *staged* upload, many commits building one file, which does need such a
    /// rule and is named in `docs/bgvql.md` §8 with it.
    ///
    /// # A ranged write reads before it writes, and that is the risky part
    ///
    /// A write that begins mid-chunk has to keep the bytes on either side of it
    /// inside that chunk, so the edge chunks are read, spliced and written back.
    /// The tests assert the **whole** file after each ranged write rather than
    /// the range that was written, because a bad splice loses the bytes nobody
    /// was looking at.
    ///
    /// # A hole is refused rather than filled
    ///
    /// Writing past the end of a file would leave a gap, and zero-filling it
    /// would be the store inventing bytes nobody wrote. A real hole is a
    /// sparse-file feature and nobody has asked for one.
    pub(crate) fn put_file(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        start: Option<u64>,
        bytes: &[u8],
    ) -> Result<Outcome> {
        let (context, table) = self.bucket(transaction, target)?;
        let path = text_identity(target.id.fixed(target.span)?, target.span)?;
        let chunks = self.chunk_table(transaction, &context, table, target.span)?;
        let id = target.id.fixed(target.span)?.clone();
        let at = RecordAddress::new(context.namespace, context.database, table, id.clone());

        let held = match start {
            // No offset: the file is replaced. The old one goes first, because a
            // shorter file written over a longer one would otherwise keep the
            // tail of the old one — chunks nothing describes and nothing would
            // ever read, until a later write made the count long enough to reach
            // them again.
            None => {
                self.clear_chunks(transaction, &context, chunks, table, &id)?;
                bytes.to_vec()
            }
            // An offset — including zero. `START 0` writes at the beginning and
            // keeps whatever lies past the bytes given, which is what "at an
            // offset" means; replacing the file is what leaving `START` out is
            // for, and one spelling per thing.
            Some(offset) => {
                let existing = match transaction.get(&at)? {
                    Some(payload) => {
                        self.whole_file(transaction, &context, chunks, path, &payload, target.span)?
                    }
                    None => Vec::new(),
                };
                let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                if offset > existing.len() {
                    return Err(Error::WriteWouldLeaveAHole {
                        path: path.to_owned(),
                        at: offset,
                        size: existing.len(),
                        span: target.span,
                    });
                }
                let mut spliced = existing;
                let end = offset.saturating_add(bytes.len());
                if end > spliced.len() {
                    spliced.resize(end, 0);
                }
                if let Some(slice) = spliced.get_mut(offset..end) {
                    slice.copy_from_slice(bytes);
                }
                self.clear_chunks(transaction, &context, chunks, table, &id)?;
                spliced
            }
        };

        let mut ordinal: u32 = 0;
        for part in held.chunks(CHUNK) {
            transaction.put(
                RecordAddress::new(
                    context.namespace,
                    context.database,
                    chunks,
                    chunk_id(path, ordinal),
                ),
                encode_payload(&Value::Bytes(part.to_vec())).into_bytes(),
            );
            ordinal = ordinal.saturating_add(1);
        }

        let metadata = Value::Object(
            [
                (FIELD_SIZE.to_owned(), count(held.len())),
                (
                    FIELD_CHUNKS.to_owned(),
                    count(usize::try_from(ordinal).unwrap_or(usize::MAX)),
                ),
                (FIELD_UPDATED.to_owned(), written_at()),
            ]
            .into_iter()
            .collect(),
        );
        transaction.put(at, encode_payload(&metadata).into_bytes());
        Ok(Outcome::Done)
    }

    /// `READ media:'/logo.png'` — the file's bytes, or `NONE` if there is none.
    ///
    /// Read by point reads rather than by a scan: the metadata says how many
    /// chunks there are and it was written in the same commit as the chunks, so
    /// the count is not a guess that a scan would be checking.
    /// The store's log as a backup file, from `from` or from the beginning.
    ///
    /// # Why this answers with the bytes rather than writing a file
    ///
    /// Because a node that is **serving** holds the store, and this store is
    /// single-writer (ADR-0007) — so no second process can open it to take a
    /// backup. Asking the node is the only way, and the language is how this
    /// store is asked (ADR-0011 §6): the HTTP route is a surface over this
    /// statement rather than a second implementation of it, and the CLI and the
    /// wire protocol get it without one either.
    ///
    /// # What a concurrent write does to it
    ///
    /// Nothing, and that was already true: `write_from` fixes the log's tail
    /// **before** reading the first record and stops there, so a write that
    /// lands mid-backup is honestly outside the file rather than half inside it.
    /// The tail is written into the header, which is what makes "outside"
    /// checkable rather than a claim.
    ///
    /// # The cost, stated
    ///
    /// The whole file is materialised, because a statement answers with a value.
    /// `FROM` is what bounds it — an incremental backup carries the records since
    /// a sequence — and a streaming answer is named in `docs/bgvql.md` §8 rather
    /// than left to be discovered by whoever backs up a large store first.
    pub(crate) fn backup(&self, from: Option<u64>) -> Result<Outcome> {
        let mut held = Vec::new();
        bgv_db_backup::write_from(
            self.store,
            &mut held,
            bgv_db_types::Sequence::new(from.unwrap_or(1).max(1)),
        )
        .map_err(|error| Error::BackupFailed {
            reason: error.to_string(),
        })?;
        Ok(Outcome::Value(Value::Bytes(held)))
    }

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
        let (context, table) = self.bucket(transaction, target)?;
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
    fn whole_file(
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

    /// Remove a file's chunks, given what its metadata says it has.
    ///
    /// Called before a write and before a delete. Silent when there is no file:
    /// a bucket with nothing at that path has no chunks to remove, which is not
    /// a condition worth an error.
    pub(crate) fn clear_chunks(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        chunks: TableId,
        bucket: TableId,
        id: &RecordId,
    ) -> Result<()> {
        let RecordId::Text(path) = id else {
            return Ok(());
        };
        let address = RecordAddress::new(context.namespace, context.database, bucket, id.clone());
        let Some(payload) = transaction.get(&address)? else {
            return Ok(());
        };
        let held = decode_payload(&payload)?;
        for ordinal in 0..chunk_count(&held) {
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                chunks,
                chunk_id(path, ordinal),
            ));
        }
        Ok(())
    }

    /// Resolve a target that must **not** name a bucket.
    ///
    /// The one guard behind `CREATE`, `UPDATE` and `SET`. A bucket's records are
    /// metadata describing bytes the store holds, so a record written by hand is
    /// a record that can lie — a size that disagrees with the file, a chunk count
    /// pointing at chunks nobody wrote. Nothing would ever catch it, because
    /// there is nothing to catch it against.
    ///
    /// `DELETE` is deliberately not here: removing a file is how a file is
    /// removed, and it takes the chunks with it.
    pub(crate) fn writable(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<(Context, RecordAddress)> {
        let (context, address) = self.address(transaction, target)?;
        if Catalog::new(transaction)
            .table(address.table)?
            .is_some_and(|found| found.bucket)
        {
            return Err(Error::NotWrittenByHand {
                table: target.table.name.text.clone(),
                span: target.span,
            });
        }
        Ok((context, address))
    }

    /// Remove a file's chunks when the table being deleted from is a bucket.
    ///
    /// Called by `DELETE`, which is the statement that removes a file — so the
    /// bytes go with the metadata in one commit, and a bucket cannot accumulate
    /// chunks nothing describes.
    pub(crate) fn clear_file(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<()> {
        let (context, table) = self.resolve_table(transaction, &target.table)?;
        let named = Catalog::new(transaction)
            .table(table)?
            .filter(|found| found.bucket)
            .map(|found| Catalog::chunks_named(&found.name));
        let Some(named) = named else {
            return Ok(());
        };
        let Some(chunks) =
            Catalog::new(transaction).table_id(context.namespace, context.database, &named)?
        else {
            return Ok(());
        };
        self.clear_chunks(
            transaction,
            &context,
            chunks,
            table,
            target.id.fixed(target.span)?,
        )
    }

    /// Resolve a target that must name a bucket.
    ///
    /// A `PUT` against an ordinary table is refused here rather than writing a
    /// record that looks like a file: the two are the same shape on disk, and a
    /// table that gained file semantics because somebody used the wrong verb is
    /// a state nothing could later tell apart.
    pub(crate) fn bucket(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<(Context, TableId)> {
        let (context, table) = self.resolve_table(transaction, &target.table)?;
        let definition = Catalog::new(transaction).table(table)?;
        if !definition.is_some_and(|found| found.bucket) {
            return Err(Error::NotABucket {
                table: target.table.name.text.clone(),
                span: target.span,
            });
        }
        Ok((context, table))
    }

    /// The table a bucket's chunks live in.
    fn chunk_table(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        bucket: TableId,
        span: Span,
    ) -> Result<TableId> {
        let named = Catalog::new(transaction)
            .table(bucket)?
            .map(|found| Catalog::chunks_named(&found.name));
        let Some(named) = named else {
            return Err(Error::Unknown {
                entity: "table",
                name: "a bucket".to_owned(),
                span,
            });
        };
        Catalog::new(transaction)
            .table_id(context.namespace, context.database, &named)?
            .ok_or(Error::Unknown {
                entity: "the table a bucket's chunks live in",
                name: named,
                span,
            })
    }
}

/// A chunk's identity: the file's path, then its ordinal.
///
/// Big-endian and fixed-width so the identities sort in the order the chunks are
/// read in — the store orders record identities, so this is the store's ordering
/// and not one this module maintains.
fn chunk_id(path: &str, ordinal: u32) -> RecordId {
    let mut bytes = path.as_bytes().to_vec();
    bytes.extend_from_slice(&ordinal.to_be_bytes());
    RecordId::Bytes(bytes)
}

/// A file is named by a path, so its identity is text.
fn text_identity(id: &RecordId, span: Span) -> Result<&str> {
    match id {
        RecordId::Text(path) => Ok(path),
        _ => Err(Error::FileNeedsAPath { span }),
    }
}

/// How many chunks a metadata record says a file has.
///
/// Absent or malformed reads as none, which answers an empty file rather than
/// failing: the alternative is an error whose only cause is a record this module
/// wrote, and a metadata record it did not write cannot exist.
fn chunk_count(metadata: &Value) -> u32 {
    let Value::Object(fields) = metadata else {
        return 0;
    };
    match fields.get(FIELD_CHUNKS) {
        Some(Value::Number(bgv_db_types::Number::Integer(held))) => {
            u32::try_from(*held).unwrap_or(0)
        }
        _ => 0,
    }
}

/// When a file was written.
///
/// `NONE` if the clock is unreadable rather than a failure: a file whose bytes
/// landed and whose timestamp did not is still a file, and refusing the write
/// would lose the thing the caller actually asked to keep.
fn written_at() -> Value {
    let Ok(since) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return Value::None;
    };
    let Ok(seconds) = i64::try_from(since.as_secs()) else {
        return Value::None;
    };
    bgv_db_types::Datetime::new(seconds, since.subsec_nanos()).map_or(Value::None, Value::Datetime)
}

/// A count, as the value system holds one.
fn count(held: usize) -> Value {
    Value::Number(bgv_db_types::Number::Integer(
        i64::try_from(held).unwrap_or(i64::MAX),
    ))
}
