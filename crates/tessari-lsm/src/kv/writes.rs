//! Writes: a batch, a group of batches, a range delete.
//!
//! A batch with preconditions commits inside a serializable engine transaction, so what the
//! preconditions read holds where the batch lands (the module documentation says why that needs
//! no lock). The engine's `Conflict` on commit means another commit moved a key this one read or
//! wrote; the batch is then judged again from a fresh snapshot.

use tessari_kv::{KeyRange, Keyspace, Result, ScanRequest, Value, WriteBatch, WriteOp};
use tessarikv::{
    Durability as EngineDurability, Isolation, ReadOptions, Transaction, TransactionOptions,
    WriteOptions,
};

use super::TessariKvBackend;
use super::errors::{conflict, from_batch, from_engine};
use super::reads::Span;

/// How many keys one pass of a range delete with no end removes.
const DELETE_STRIDE: usize = 1024;

/// What judging a batch inside a transaction found.
enum Judged {
    /// Its preconditions hold and its writes are staged.
    Staged,
    /// A precondition does not hold; nothing of it is staged.
    Refused(tessari_kv::Error),
}

fn write_options(synced: bool) -> WriteOptions {
    let mut options = WriteOptions::default();
    options.durability = if synced {
        EngineDurability::Sync
    } else {
        EngineDurability::Buffered
    };
    options
}

fn serializable() -> TransactionOptions {
    let mut options = TransactionOptions::default();
    options.isolation = Isolation::Serializable;
    options
}

impl TessariKvBackend {
    /// Lands `batch` whole when its preconditions hold, synced when `synced`.
    pub(super) async fn land(&self, batch: WriteBatch, synced: bool) -> Result<()> {
        let options = write_options(synced);
        if batch.preconditions().is_empty() {
            let engine = self.engine_batch(&batch)?;
            self.db
                .write(&engine, &options)
                .await
                .map_err(|error| from_engine(&error))?;
            return Ok(());
        }
        loop {
            let mut transaction = self.begin()?;
            if let Judged::Refused(failure) = self.judge(&mut transaction, &batch).await? {
                return Err(failure);
            }
            match transaction.commit(&options).await {
                Ok(_) => return Ok(()),
                // Another commit moved a key this one read or wrote: judge it again.
                Err(tessarikv::Error::Conflict) => {}
                Err(error) => return Err(from_engine(&error)),
            }
        }
    }

    /// Lands every batch of `batches` up to the first whose preconditions fail, each judged
    /// after the ones before it, in one commit and one sync.
    pub(super) async fn land_group(&self, batches: Vec<WriteBatch>) -> (usize, Result<()>) {
        let options = write_options(self.durability.syncs());
        loop {
            let mut transaction = match self.begin() {
                Ok(transaction) => transaction,
                Err(failure) => return (0, Err(failure)),
            };
            let mut landed = 0_usize;
            let mut stopped = Ok(());
            for batch in &batches {
                match self.judge(&mut transaction, batch).await {
                    Ok(Judged::Staged) => landed = landed.saturating_add(1),
                    Ok(Judged::Refused(failure)) | Err(failure) => {
                        stopped = Err(failure);
                        break;
                    }
                }
            }
            if landed == 0 {
                return (0, stopped);
            }
            match transaction.commit(&options).await {
                Ok(_) => return (landed, stopped),
                Err(tessarikv::Error::Conflict) => {}
                Err(error) => return (0, Err(from_engine(&error))),
            }
        }
    }

    /// Deletes exactly the keys of `range`.
    pub(super) async fn remove_range(&self, keyspace: Keyspace, range: &KeyRange) -> Result<()> {
        if range.is_provably_empty() {
            return Ok(());
        }
        let id = self.keyspace(keyspace)?;
        let span = Span::of(range);
        let options = write_options(self.durability.syncs());
        if let Some(end) = &span.end {
            let mut engine = tessarikv::WriteBatch::new();
            engine
                .delete_range(id, &span.start, end)
                .map_err(|error| from_batch(&error))?;
            self.db
                .write(&engine, &options)
                .await
                .map_err(|error| from_engine(&error))?;
            return Ok(());
        }
        // A range with no last key has no end to give a range tombstone, so it goes in bounded
        // strides of point deletes, each re-read from the range's start.
        loop {
            let request = ScanRequest::new(keyspace, range.clone()).with_limit(DELETE_STRIDE);
            let doomed = self.read_range(&request, false).await?;
            let reached = doomed.len();
            if reached == 0 {
                return Ok(());
            }
            let mut engine = tessarikv::WriteBatch::new();
            for (key, _) in doomed {
                engine
                    .delete(id, key.as_slice())
                    .map_err(|error| from_batch(&error))?;
            }
            self.db
                .write(&engine, &options)
                .await
                .map_err(|error| from_engine(&error))?;
            if reached < DELETE_STRIDE {
                return Ok(());
            }
        }
    }

    fn begin(&self) -> Result<Transaction<'_>> {
        self.db
            .begin_with(&serializable())
            .map_err(|error| from_engine(&error))
    }

    /// Checks `batch`'s preconditions inside `transaction` (which sees what earlier batches of
    /// it staged) and stages its writes when they hold.
    async fn judge(&self, transaction: &mut Transaction<'_>, batch: &WriteBatch) -> Result<Judged> {
        for precondition in batch.preconditions() {
            let id = self.keyspace(precondition.keyspace())?;
            let observed = transaction
                .get(id, precondition.key().as_slice(), &ReadOptions::default())
                .await
                .map_err(|error| from_engine(&error))?
                .map(Value::new);
            if !precondition.is_satisfied_by(observed.as_ref()) {
                return Ok(Judged::Refused(conflict(
                    precondition.keyspace(),
                    precondition.key(),
                )));
            }
        }
        // Every keyspace is resolved before anything is staged, so an unknown one refuses the
        // batch instead of splitting it.
        let targets = batch
            .ops()
            .iter()
            .map(|op| self.keyspace(op.keyspace()))
            .collect::<Result<Vec<_>>>()?;
        for (op, id) in batch.ops().iter().zip(targets) {
            match op {
                WriteOp::Put { key, value, .. } => transaction
                    .put(id, key.as_slice(), value.as_slice())
                    .map_err(|error| from_engine(&error))?,
                WriteOp::Delete { key, .. } => transaction
                    .delete(id, key.as_slice())
                    .map_err(|error| from_engine(&error))?,
            }
        }
        Ok(Judged::Staged)
    }

    /// `batch`'s writes as one engine batch, every keyspace resolved first.
    fn engine_batch(&self, batch: &WriteBatch) -> Result<tessarikv::WriteBatch> {
        let mut engine = tessarikv::WriteBatch::new();
        for op in batch.ops() {
            self.keyspace(op.keyspace())?;
        }
        for op in batch.ops() {
            let id = self.keyspace(op.keyspace())?;
            match op {
                WriteOp::Put { key, value, .. } => engine.put(id, key.as_slice(), value.as_slice()),
                WriteOp::Delete { key, .. } => engine.delete(id, key.as_slice()),
            }
            .map_err(|error| from_batch(&error))?;
        }
        Ok(engine)
    }
}
