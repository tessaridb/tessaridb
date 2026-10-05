//! Writing a value as JSON.
//!
//! The writer lives in `tessari_types::json`, beside the reader, so this surface
//! and the language's `json::encode` write one mapping (ADR-0116 D1). Its tests
//! stay here, where it was first written.

pub(crate) use tessari_types::json::{Names, string, string_literal, write};

#[cfg(test)]
mod tests;
