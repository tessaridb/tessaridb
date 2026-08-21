//! Shared value types for `bgv-db`.
//!
//! This crate is a dependency leaf: it defines the identifiers, value types and
//! newtypes every other crate in the workspace speaks in, and depends on nothing
//! inside the workspace itself.
//!
//! Nothing is implemented yet. The crate exists so the workspace layout, lint
//! configuration and CI pipeline are in place before the first real type lands.

#![forbid(unsafe_code)]
