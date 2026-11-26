//! The columnar projection: per-column chunks beside the versioned row.
//!
//! The row stays the canonical, versioned value (ADR-0137); this module is
//! its auxiliary projection — one append-ordered chunk per (table, field),
//! written inside the record's own commit batch, sealed at a boundary, and
//! carrying its encoding, its nulls as a validity bitmap, its head
//! statistics, and the record-id index that maps decoded runs back to
//! identities.
//!
//! Design authorities: the SG1 worksheet (`plans/2026-11-15/155335-g082-sg1-
//! columnar-design-worksheet.md`) §4 layout, §4.4 (statistics are written by
//! the writer) and §5.1's one-batch rule, ADR-0137's row-authority decision,
//! and the SG3 plan (`plans/2026-11-15/180502-g082-sg3-skip-indexes-and-
//! pruning-plan.md`) for the pruning scan and its candidate contract.

pub(crate) mod bloom;
mod chunk;
mod codec;
pub(crate) mod heads;
mod project;
// Test-only: the production reader is scan.rs; these are the equivalence
// suite's decode helpers (the SG3 plan's 'foundation' is shared at the
// decode primitives, not by shipping unused production code).
#[cfg(test)]
mod read;
mod scan;
#[cfg(test)]
mod scan_tests;
mod writer;

#[cfg(test)]
mod chunk_tests;
#[cfg(test)]
mod codec_tests;
#[cfg(test)]
mod project_tests;

#[cfg(test)]
pub(crate) use chunk::ROWS_PER_CHUNK;
pub(crate) use project::maintain;
pub use scan::{
    ColumnarCompare, ColumnarPredicate, ColumnarScanReport, ColumnarTest, columnar_lane,
    columnar_scan, columnar_scan_partitioned,
};
