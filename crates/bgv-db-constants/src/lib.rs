//! Tunable constants for `bgv-db`.
//!
//! Every magic number in the workspace lives here, named, typed, and documented
//! with its unit and the reasoning behind its value. Business and engine code
//! never carries a bare numeric literal.

#![forbid(unsafe_code)]

/// How many times a commit re-attempts after losing the race for the committed
/// tail, before reporting contention to the caller.
///
/// Unit: attempts.
///
/// Each lost attempt means a concurrent commit landed between reading the tail
/// and applying the batch, so the conflict check has to be redone. The budget
/// is bounded rather than unlimited because a caller waiting forever is worse
/// than a caller told the store is contended: contention is information, and a
/// silent spin is not.
///
/// Eight is a starting value chosen to absorb ordinary interleaving on an
/// embedded store while still surfacing sustained contention quickly. It is
/// provisional until measured under a real write workload.
pub const MAX_COMMIT_ATTEMPTS: u32 = 8;
