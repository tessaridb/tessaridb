//! Intents: a transaction across leaders' writes, held as provisional versions
//! until its record decides them (ADR-0112 D5).
//!
//! An intent is a version under the record's own key whose provenance says it
//! is provisional. Until the reading rule of ADR-0112 D6 is built, every reader
//! treats one as invisible and reads the version under it, and every writer is
//! refused while one stands — which is what the model checked in
//! `across_model` requires of both sides before anything can prepare.

use tessari_encoding::{Provenance, StampedValue};

/// Whether a stored version is an intent rather than a value.
pub(crate) fn is_intent(version: &StampedValue) -> bool {
    version
        .provenance()
        .is_some_and(|provenance: Provenance| provenance.provisional)
}

#[cfg(test)]
mod tests;
