//! Geometry the store can be exact about.
//!
//! # The one decision everything here follows from
//!
//! Positions are snapped at ingest to a fixed integer grid, and every predicate
//! is then an integer determinant with an exact sign. That removes the reason a
//! geospatial engine normally needs an adaptive-precision floating-point kernel,
//! and it removes the tolerances that make float predicates non-transitive.
//!
//! The precision model was obligatory anyway — a store that keeps whatever float
//! arrived cannot promise a lossless read-modify-write, and overlay operations
//! over mixed precision produce output invalid by the store's own definition.
//! Making the grid integral turns that obligation into the robustness answer as
//! well. See [`grid`] for the argument and what the grid costs.
//!
//! # What this crate is not
//!
//! It holds no storage, no index and no query planning — only the computations
//! those layers stand on. The kernel being separable is what makes it testable
//! against a brute-force oracle, which is the only way geometric code is ever
//! known to be right: every wrong geospatial answer looks exactly like a right
//! one, so correctness comes from oracles and fixtures rather than from reading
//! results and finding them plausible.

pub mod accept;
pub mod bounds;
pub mod grid;
pub mod measure;
pub mod predicate;
pub mod relate;
pub mod shape;
mod witness;

pub use crate::accept::{Defect, Refused, accept};
pub use crate::bounds::Bounds;
pub use crate::grid::{Axis, OffGrid, SCALE, Snapped};
pub use crate::measure::{area, authalic_radius, distance};
pub use crate::predicate::{
    Containment, Orientation, on_segment, orientation, ring_contains, segments_cross,
    segments_meet, twice_signed_area,
};
pub use crate::relate::{contains, covered_by, covers, disjoint, equals, intersects, within};
pub use crate::shape::{Area, Loop, Shape};
