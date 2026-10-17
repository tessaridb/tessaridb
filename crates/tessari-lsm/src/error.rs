//! The backend's name in the substrate's errors.
//!
//! The engine's failures are translated by category in `kv::errors`; this is the one name they
//! carry, stable across engines, so nothing above learns which engine answered.

pub(crate) const BACKEND_NAME: &str = "lsm";
