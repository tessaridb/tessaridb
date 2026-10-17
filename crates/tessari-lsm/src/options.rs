//! The one place engine configuration is decided.
//!
//! Every option the store runs with is produced here, by the service, the tests and any tool
//! alike. A second place that builds options is a second store, because the keyspace set and
//! each keyspace's table format are part of the on-disk contract and not of a caller's taste.
//!
//! # What is set, and why
//!
//! A value with no reason is deleted, not kept in case it helps. Each one below is either
//! required for correctness or bounds a resource that is otherwise unbounded.
//!
//! - Serializable transactions, because a batch's preconditions are checked inside one (see
//!   the backend's module documentation).
//! - Every keyspace named at open, created when missing, so a store from this build reopens.
//! - One shared block cache, a whole-database ceiling on in-memory tables, and a bound on open
//!   table files: the three terms of the memory budget this module sets. They are starting
//!   points small enough to run anywhere, not measured values.
//! - A compression profile per keyspace, matching how each is read and written: metadata is
//!   read on the commit path and stays uncompressed; the log only grows and is kept, so it is
//!   compressed densely throughout (G044 C10: the log was the larger half of a series' bytes);
//!   records and index entries take a fast codec above and a dense one at the bottom, where
//!   most bytes live. The log is never point-read, so it carries no membership filter.
//! - Statistics, wired before the first load rather than after the first incident.

use std::path::Path;

use tessari_kv::Keyspace;
use tessarikv::{Compression, KeyspaceOptions};

/// What an acknowledged write is promised to survive.
///
/// Both levels are named in the API because "durable" on its own tells a caller
/// nothing. There is deliberately no level below these two: a store that buffers
/// acknowledged writes inside the process is not something a caller can reason
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// The write is synced before it is acknowledged.
    ///
    /// Intended to survive loss of power to the machine. The cost is one device
    /// sync per commit, so throughput is bounded by the device's sync rate.
    ///
    /// This is the default because the store is a system of record.
    ///
    /// The promise is only as strong as the platform underneath it: a sync a
    /// drive acknowledges from its own cache is not durability, and no software
    /// test can tell the difference. Confirming that on the hardware a store
    /// actually runs on is part of taking it into production, not part of
    /// choosing this level.
    #[default]
    PowerLossSafe,

    /// The write is in the operating system's page cache when it is
    /// acknowledged.
    ///
    /// Survives the process being killed. **Lost on power loss or a kernel
    /// panic.** Correct for data that can be recomputed, and for a test suite
    /// that has to run on every change; not correct for a system of record.
    ProcessCrashSafe,
}

impl Durability {
    /// Whether a commit at this level waits for the device.
    #[must_use]
    pub(crate) const fn syncs(self) -> bool {
        matches!(self, Self::PowerLossSafe)
    }

    /// A short stable name for logs and health output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PowerLossSafe => "power-loss-safe",
            Self::ProcessCrashSafe => "process-crash-safe",
        }
    }
}

/// How a store is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// What an acknowledged write survives.
    pub durability: Durability,
    /// The shared block cache, which also carries index and filter blocks.
    pub block_cache_bytes: usize,
    /// The ceiling on memtable memory across every region together.
    pub memtable_bytes: usize,
    /// The most files the engine may keep open at once.
    ///
    /// This bounds the table-reader term of the memory budget. The engine's own
    /// default is `-1`, meaning unlimited — which the audit found while the
    /// module above was already describing that term as "bounded by the
    /// open-file limit". It was not bounded; now it is.
    pub max_open_files: i32,
}

impl StoreConfig {
    /// Starting point for the block cache.
    ///
    /// Small enough to run on a laptop and in CI. Not a measured value.
    pub const DEFAULT_BLOCK_CACHE_BYTES: usize = 64 * 1024 * 1024;
    /// Starting point for the memtable ceiling. Not a measured value.
    pub const DEFAULT_MEMTABLE_BYTES: usize = 64 * 1024 * 1024;
    /// Starting point for the open-file bound. Not a measured value.
    ///
    /// Finite rather than the engine's unlimited default, so the table-reader
    /// term of the memory budget has a ceiling at all. The value that replaces
    /// it comes from the capacity measurement on the readiness checklist.
    pub const DEFAULT_MAX_OPEN_FILES: i32 = 1024;

    /// Open with the given durability and the default memory budget.
    #[must_use]
    pub const fn new(durability: Durability) -> Self {
        Self {
            durability,
            block_cache_bytes: Self::DEFAULT_BLOCK_CACHE_BYTES,
            memtable_bytes: Self::DEFAULT_MEMTABLE_BYTES,
            max_open_files: Self::DEFAULT_MAX_OPEN_FILES,
        }
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self::new(Durability::PowerLossSafe)
    }
}

/// Bits per key of the membership filter on a keyspace that is point-read.
const FILTER_BITS_PER_KEY: u32 = 10;

/// The table options of `keyspace`, with `memtable_bytes` of in-memory table.
pub(crate) fn keyspace_options(
    keyspace: Keyspace,
    memtable_bytes: u64,
) -> tessarikv::Result<KeyspaceOptions> {
    let (upper, bottom, filter) = match keyspace {
        Keyspace::META => (Compression::None, None, FILTER_BITS_PER_KEY),
        Keyspace::LOG => (Compression::Zstd, Some(Compression::Zstd), 0),
        _ => (
            Compression::Lz4,
            Some(Compression::Zstd),
            FILTER_BITS_PER_KEY,
        ),
    };
    KeyspaceOptions::builder()
        .memtable_bytes(memtable_bytes)
        .compression(upper)
        .bottommost_compression(bottom)
        .bloom_bits_per_key(filter)
        .build()
}

/// The engine's own record of the options a store is running with.
///
/// The engine writes this file on every open, and it is the ground truth for an
/// audit — the application's configuration struct records what the code meant to
/// set, which is a different question.
pub fn effective_options_files(store_path: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(store_path)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("OPTIONS-"))
        })
        .collect();
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_default_level_is_the_one_a_system_of_record_needs() {
        assert_eq!(StoreConfig::default().durability, Durability::PowerLossSafe);
        assert_eq!(Durability::default(), Durability::PowerLossSafe);
    }

    #[test]
    fn each_level_has_a_stable_name() {
        assert_eq!(Durability::PowerLossSafe.name(), "power-loss-safe");
        assert_eq!(Durability::ProcessCrashSafe.name(), "process-crash-safe");
    }

    #[test]
    fn each_keyspace_is_compressed_for_how_it_is_read() {
        let meta = keyspace_options(Keyspace::META, 1 << 20).unwrap();
        assert_eq!(meta.compression(), Compression::None);
        assert_eq!(meta.bottommost_compression(), None);
        let log = keyspace_options(Keyspace::LOG, 1 << 20).unwrap();
        assert_eq!(log.compression(), Compression::Zstd);
        assert_eq!(log.bloom_bits_per_key(), 0, "the log is never point-read");
        for keyspace in [Keyspace::DATA, Keyspace::INDEX] {
            let options = keyspace_options(keyspace, 1 << 20).unwrap();
            assert_eq!(options.compression(), Compression::Lz4, "{keyspace}");
            assert_eq!(options.bottommost_compression(), Some(Compression::Zstd));
            assert_eq!(options.memtable_bytes(), 1 << 20);
        }
    }
}
