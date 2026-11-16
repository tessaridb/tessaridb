//! What the crate refuses, in one place.

use thiserror::Error;

/// The result of every fallible call here.
pub type Result<T> = std::result::Result<T, Error>;

/// Why a frame or a call did not proceed.
#[derive(Debug, Error)]
pub enum Error {
    /// A frame's bytes do not say what the protocol says they must.
    #[error("malformed: {0}")]
    Malformed(&'static str),
    /// A declared length above the ceiling, refused before any allocation.
    #[error("a frame declares {0} bytes, above the {CEILING} the surface accepts")]
    Ceiling(u32),
    /// The socket underneath failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The most one frame may carry, in both directions: the same ceiling the
/// node's own wire answers with, checked before anything is allocated.
pub const CEILING: u32 = 16 * 1024 * 1024;
