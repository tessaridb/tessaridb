//! How many log records this node keeps, and who decided it (ADR-0094 D2).
//!
//! Three places can say, and the most specific wins: a `DEFINE NODE RETAIN`
//! stored on this node, the process default a serving node is started with
//! (`TESSARIDB_RETAIN_RECORDS`), and the engine's constant. All three are local
//! to this machine (ADR-0018): none of them is in the log, a backup, or a
//! follower.

use std::sync::{Arc, OnceLock};

use tessari_encoding::{LogRetentionKey, StoreKey, StoreValue};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

use crate::error::Result;
use crate::store::Store;

/// How many records a log keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// The newest this many records of each log.
    Keep(Sequence),
    /// Every record, for ever.
    Unbounded,
}

/// Which of the three places the effective retention came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionSource {
    /// `DEFINE NODE RETAIN n RECORDS` or `RETAIN NONE`, stored on this node.
    Statement,
    /// The default this process was started with.
    Environment,
    /// [`tessari_constants::DEFAULT_LOG_RETENTION_RECORDS`].
    Default,
}

impl RetentionSource {
    /// The word `INFO FOR NODE` reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Statement => "statement",
            Self::Environment => "environment",
            Self::Default => "default",
        }
    }
}

/// The default this process applies where no statement said otherwise.
///
/// Per process and never persisted, like the vault's unseal period: it is how
/// this node was started, and a restarted node started differently should
/// behave differently.
#[derive(Debug, Default)]
pub(crate) struct ProcessRetention(OnceLock<Retention>);

impl ProcessRetention {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl Store {
    /// What `DEFINE NODE RETAIN` stored on this node, if anything did.
    ///
    /// `None` is *never said*, which is different from
    /// [`Retention::Unbounded`]: an operator who wrote `RETAIN NONE` chose an
    /// unbounded log, and that choice must outlive a restart with a different
    /// default.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn log_retention(&self) -> Result<Option<Retention>> {
        let key = LogRetentionKey.encode();
        match self.backend().get(LogRetentionKey::keyspace(), &key)? {
            // An empty value is the stored `RETAIN NONE`. Stores written before
            // ADR-0094 deleted the key instead, which now reads as never said
            // — the default they get is the one the owner chose.
            Some(value) if value.is_empty() => Ok(Some(Retention::Unbounded)),
            Some(value) => Ok(Some(Retention::Keep(Sequence::decode(value.as_slice())?))),
            None => Ok(None),
        }
    }

    /// Store how many log records this node keeps: `Some(n)` for a window,
    /// `None` for `RETAIN NONE`, an unbounded log chosen on purpose.
    ///
    /// Written outside any transaction, because it is a fact about this machine
    /// rather than about the data, and the log does not carry it (ADR-0018). A
    /// retention that replicated would be inherited by whoever restored a backup
    /// and adopted by every follower of whoever set it.
    ///
    /// # Errors
    ///
    /// Returns the backend's own failure.
    pub fn set_log_retention(&self, keep: Option<Sequence>) -> Result<()> {
        let key = LogRetentionKey.encode();
        let value = keep.map_or_else(|| tessari_kv::Value::from(Vec::new()), |keep| keep.encode());
        self.backend()
            .apply(WriteBatch::new().put(LogRetentionKey::keyspace(), key, value))?;
        Ok(())
    }

    /// Set the default this process applies where no statement said otherwise.
    ///
    /// Taken once, when a node starts serving; a second call changes nothing,
    /// because a default that moved under a running node would be a setting
    /// with no statement behind it.
    pub fn retain_by_default(&self, retention: Retention) {
        // Ignored rather than refused: the first value is the one the process
        // was started with, and a later call has nothing to add to it.
        let _first = self.process_retention().0.set(retention);
    }

    /// The retention this node applies, and where it came from.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored value cannot be read.
    pub fn effective_retention(&self) -> Result<(Retention, RetentionSource)> {
        if let Some(stored) = self.log_retention()? {
            return Ok((stored, RetentionSource::Statement));
        }
        Ok(self.process_retention().0.get().map_or(
            (
                Retention::Keep(Sequence::new(
                    tessari_constants::DEFAULT_LOG_RETENTION_RECORDS,
                )),
                RetentionSource::Default,
            ),
            |given| (*given, RetentionSource::Environment),
        ))
    }
}
