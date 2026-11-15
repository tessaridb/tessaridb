//! The columnar projection: per-column chunks beside the versioned row.
//!
//! The row stays the canonical, versioned value (ADR-0137); this module is
//! its auxiliary projection — one append-ordered chunk per (table, field),
//! written inside the record's own commit batch, sealed at a boundary, and
//! carrying its encoding, its nulls as a validity bitmap, and the record-id
//! index that maps decoded runs back to identities.
//!
//! Design authorities: the SG1 worksheet (`plans/2026-11-15/155335-g082-sg1-
//! columnar-design-worksheet.md`) §4 layout and §5.1's one-batch rule, and
//! ADR-0137's row-authority decision. This wave (SG2 T2.1) lands the codec
//! and the chunk payload only; the commit-batch projection lands in T2.2
//! beside `index::maintain`, the same-batch pattern the engine already runs.

mod bloom;
mod chunk;
mod codec;
mod project;
// Test-only until SG3's scan becomes the production reader of these helpers.
#[cfg(test)]
mod read;
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
