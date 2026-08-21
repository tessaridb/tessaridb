//! The one place engine configuration is decided.
//!
//! Every option the store runs with is produced here — by the service, by the
//! tests and by any future tool alike. A second place that builds options is a
//! second store, because the comparator, the region set and the table format are
//! part of the on-disk contract and not of the caller's preferences.
//!
//! # What is set, and why each value exists
//!
//! A value with no reason is deleted, not kept in case it helps. Everything set
//! below is set for one of exactly two reasons: it is required for correctness,
//! or it bounds a resource that is otherwise unbounded.
//!
//! **Correctness**
//!
//! - Atomic flush. A batch here writes a record in one region and the position
//!   that accounts for it in another. Without atomic flush the regions flush
//!   independently and recovery can restore one past the other, leaving a record
//!   that nothing will ever reconcile.
//! - Tolerate a corrupted trailing WAL record, *and* verify the WAL set against
//!   the manifest. This works as a pair and not as two settings. At
//!   [`Durability::PowerLossSafe`] every acknowledged write is synced before it
//!   is acknowledged, so a torn *trailing* record is by construction one nobody
//!   was told about — dropping it loses nothing that was promised, while
//!   refusing to open on it would turn an ordinary power loss into an outage.
//!   What must never be silent is a *missing* log file, which is real loss, and
//!   that is what the manifest check catches.
//! - No prefix extractor. Setting one turns every plain seek into a prefix seek,
//!   which silently truncates a scan that crosses the prefix boundary. Our key
//!   grammar scans across record versions, so this would be wrong.
//! - The default byte-order comparator. The key grammar is designed so that byte
//!   order *is* logical order; a custom comparator would be a data migration to
//!   undo and would buy nothing.
//!
//! **Bounded resources**
//!
//! - One shared block cache across every region. Per-region caches fragment the
//!   budget so a hot region evicts while a cold one holds memory it is not
//!   using.
//! - Index and filter blocks charged into that same cache. Left uncharged they
//!   are a second budget with no ceiling that grows with the file count.
//! - A whole-database memtable ceiling, so the region count cannot multiply the
//!   memtable term without a bound.
//!
//! # The memory budget as a sum
//!
//! ```text
//! total ≈ memtable ceiling            (db_write_buffer_size)
//!       + block cache                 (shared, and charged with index/filter)
//!       + table readers               (bounded by the open-file limit)
//!       + iterator-pinned blocks      (bounded by scan concurrency)
//! ```
//!
//! The first two are the terms this module sets. They are *starting points*
//! chosen to be small enough to run anywhere, not measured values — the
//! measurement that replaces them is its own task, and until it lands the honest
//! statement is that these numbers are placeholders with a ceiling, not a
//! tuning.

use std::path::Path;

use bgv_db_kv::Keyspace;
use rocksdb::{
    BlockBasedOptions, Cache, ChecksumType, DBCompressionType, DBRecoveryMode, Options,
    WriteOptions,
};

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
    /// The write options that deliver this level.
    #[must_use]
    pub(crate) fn write_options(self) -> WriteOptions {
        let mut options = WriteOptions::default();
        options.set_sync(self == Self::PowerLossSafe);
        options
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

/// Database-wide options.
///
/// `create` decides whether a missing store is created. It is a parameter rather
/// than a constant because creating a store and opening an existing one are
/// different intentions, and conflating them is how a deployment ordering bug
/// turns into an empty store that serves empty results.
pub(crate) fn database_options(config: &StoreConfig, create: bool) -> Options {
    let mut options = Options::default();
    options.create_if_missing(create);
    options.create_missing_column_families(create);

    // A batch spans regions and carries an invariant across them. This is what
    // makes that invariant survive a crash rather than only a reader.
    options.set_atomic_flush(true);

    options.set_wal_recovery_mode(DBRecoveryMode::TolerateCorruptedTailRecords);
    options.set_track_and_verify_wals_in_manifest(true);

    options.set_db_write_buffer_size(config.memtable_bytes);

    // The engine's default is unlimited, which leaves the table-reader term of
    // the memory budget with no ceiling. Found by auditing the written OPTIONS
    // file against the module's own claim that the term was bounded.
    options.set_max_open_files(config.max_open_files);

    // Wired before the first load rather than after the first incident.
    options.enable_statistics();

    options
}

/// Options for one region.
///
/// Each region gets a profile matching how it is actually read and written. That
/// difference is the only admissible reason for the regions to be separate at
/// all — anything that differs only by which entity it holds belongs in the key
/// prefix instead.
fn region_options(keyspace: Keyspace, cache: &Cache) -> Options {
    let mut options = Options::default();
    options.set_compression_per_level(&compression_per_level(keyspace));
    if let Some(bottom) = bottommost_compression(keyspace) {
        options.set_bottommost_compression_type(bottom);
    }
    options.set_block_based_table_factory(&table_options(keyspace, cache));

    // Pinned at the value the engine already resolves to, so that a release
    // changing its default cannot change how often this store rewrites itself
    // without anyone deciding to. See SST_REWRITE_SECONDS.
    options.set_ttl(SST_REWRITE_SECONDS);
    options
}

/// How hard each level is compressed.
///
/// The top levels are left uncompressed because they are rewritten constantly
/// and hold the least data; the bottom is compressed hardest because it holds
/// most of the bytes and is rewritten least.
fn compression_per_level(keyspace: Keyspace) -> [DBCompressionType; LEVELS] {
    const NONE: DBCompressionType = DBCompressionType::None;
    const FAST: DBCompressionType = DBCompressionType::Lz4;
    const DENSE: DBCompressionType = DBCompressionType::Zstd;

    match keyspace {
        // Metadata is a handful of keys read on the commit path. Compressing it
        // costs CPU on the hottest read in the store and saves nothing worth
        // measuring.
        Keyspace::META => [NONE; LEVELS],
        // The log is written once, read sequentially by a follower, and dropped
        // by retention rather than rewritten. Dense compression at the bottom
        // would pay to compact bytes that are about to be discarded.
        Keyspace::LOG => [NONE, NONE, FAST, FAST, FAST, FAST, FAST],
        _ => [NONE, NONE, FAST, FAST, FAST, DENSE, DENSE],
    }
}

/// The compression the last level settles at, where it differs from the ladder.
fn bottommost_compression(keyspace: Keyspace) -> Option<DBCompressionType> {
    match keyspace {
        Keyspace::META | Keyspace::LOG => None,
        _ => Some(DBCompressionType::Zstd),
    }
}

/// The engine's default level count. Changing it is a reopen, so the ladder
/// above is sized to it deliberately rather than by coincidence.
///
/// Confirmed against the running release rather than assumed: the written
/// OPTIONS file reports `num_levels=7`.
const LEVELS: usize = 7;

/// How old a file may get before compaction rewrites it: thirty days.
///
/// This is not a value chosen for a workload — it is the value the engine was
/// **already applying**, discovered by auditing the OPTIONS file it writes. The
/// header declares a sentinel default, and the engine resolves that sentinel to
/// thirty days for leveled compaction, so reading the header alone says "unset"
/// while the store rewrites every file monthly.
///
/// Pinning it changes nothing today. What it changes is that the rewrite is now
/// a decision that survives a release changing its default, and that the
/// alternative — disabling it, trading tombstone and space reclamation for less
/// background writing on cold data — is a question the readiness checklist asks
/// with a measurement rather than a question nobody knew existed.
const SST_REWRITE_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Bits per key for the membership filter.
const FILTER_BITS_PER_KEY: f64 = 10.0;

fn table_options(keyspace: Keyspace, cache: &Cache) -> BlockBasedOptions {
    let mut table = BlockBasedOptions::default();
    table.set_block_cache(cache);

    // Pinned at the release's current default. A checksum is part of the format
    // a file is written in, so a release changing this default would change how
    // new files are written while old ones stay readable — a divergence with no
    // symptom until something needs to explain why two files differ.
    table.set_checksum_type(ChecksumType::XXH3);

    // Charged into the one cache above rather than growing beside it.
    table.set_cache_index_and_filter_blocks(true);
    table.set_pin_l0_filter_and_index_blocks_in_cache(true);

    // A membership filter answers "is this key here" without reading the file,
    // which is what a point read needs. The log is never point-read, so a filter
    // there is bytes and CPU spent on a question nobody asks.
    if keyspace != Keyspace::LOG {
        table.set_bloom_filter(FILTER_BITS_PER_KEY, false);
    }
    table
}

/// The regions a store contains, with their options.
///
/// The set is fixed. It does not grow with tables, namespaces or tenants — those
/// are key prefixes — because every region costs memtable memory and background
/// scheduling whether or not anything writes to it.
pub(crate) fn regions(cache: &Cache) -> Vec<(&'static str, Options)> {
    let mut regions: Vec<(&'static str, Options)> = Keyspace::ALL
        .iter()
        .map(|keyspace| (keyspace.name(), region_options(*keyspace, cache)))
        .collect();

    // The engine always creates a region called `default`, whether or not
    // anything uses it — and this store never does. Left alone it opens on
    // engine defaults, which means its own block cache: a second cache beside
    // the one this module calls the only one. It holds nothing and would never
    // fill, so the cost is not the point; the point is that "one shared cache"
    // is either true or it is a sentence. It is given the metadata profile,
    // being the cheapest, and it stays empty.
    regions.push((DEFAULT_REGION, region_options(Keyspace::META, cache)));
    regions
}

/// The region the engine creates on its own, which this store never writes to.
pub(crate) const DEFAULT_REGION: &str = "default";

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
    fn the_compression_ladder_covers_every_level_and_never_compresses_the_top() {
        for keyspace in Keyspace::ALL {
            let ladder = compression_per_level(*keyspace);
            assert_eq!(ladder.len(), LEVELS, "{keyspace}");
            assert_eq!(
                ladder[0],
                DBCompressionType::None,
                "{keyspace}: the newest level is rewritten constantly"
            );
        }
    }

    #[test]
    fn metadata_is_never_compressed_because_it_is_read_on_the_commit_path() {
        assert!(
            compression_per_level(Keyspace::META)
                .iter()
                .all(|level| *level == DBCompressionType::None)
        );
        assert_eq!(bottommost_compression(Keyspace::META), None);
    }

    #[test]
    fn the_log_stays_on_the_cheap_codec_all_the_way_down() {
        let ladder = compression_per_level(Keyspace::LOG);
        assert!(
            ladder.iter().all(|level| *level != DBCompressionType::Zstd),
            "the log is discarded by retention, not compacted for density"
        );
        assert_eq!(bottommost_compression(Keyspace::LOG), None);
    }

    #[test]
    fn data_and_index_compress_hardest_where_most_bytes_live() {
        for keyspace in [Keyspace::DATA, Keyspace::INDEX] {
            let ladder = compression_per_level(keyspace);
            assert_eq!(ladder[LEVELS - 1], DBCompressionType::Zstd, "{keyspace}");
            assert_eq!(
                bottommost_compression(keyspace),
                Some(DBCompressionType::Zstd),
                "{keyspace}"
            );
        }
    }

    #[test]
    fn every_keyspace_becomes_one_region_and_the_engine_s_own_region_is_managed_too() {
        let cache = Cache::new_lru_cache(1024 * 1024);
        let regions = regions(&cache);
        let names: Vec<&str> = regions.iter().map(|(name, _)| *name).collect();
        // `default` is created by the engine whether or not anything uses it.
        // Managing it is what makes "one shared block cache" true rather than
        // nearly true; the audit found it opening on its own defaults.
        assert_eq!(names, ["meta", "data", "index", "log", DEFAULT_REGION]);
    }
}
