use super::*;

/// One stored record that disagrees with what its table declares now.
///
/// An **answer**, not a refusal, which is the whole reason this type exists
/// beside [`Error`]: a check that raised on the first disagreement would make an
/// operator fix a table one statement at a time, and the count is the thing they
/// need before they can decide whether to fix the data or the declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The record's identity, as a statement would name it.
    pub record: String,
    /// The field the disagreement is about.
    pub field: String,
    /// Which rule it broke, as one stable word.
    ///
    /// Stable because a caller scripting a repair matches on this and reads the
    /// detail; a message is written for a person and may be reworded, and a
    /// caller that had to parse one would break when it was.
    pub rule: &'static str,
    /// The disagreement as the store words it when it refuses a write.
    ///
    /// The same sentence, so that a check run before a tightening statement and
    /// the tightening statement's own refusal cannot describe the same record
    /// differently.
    pub detail: String,
}

impl Violation {
    /// The answer shape for a refusal the schema pass produced.
    ///
    /// `None` for anything else, which cannot arise from [`check`] and is not
    /// asserted away: a variant added there and forgotten here would otherwise
    /// become a violation this reports as clean.
    pub(super) fn of(refusal: &Error) -> Option<Self> {
        let (record, field, rule) = match refusal {
            Error::MissingRequiredField { record, field, .. } => {
                (record.clone(), field.clone(), "required")
            }
            Error::UndeclaredField { record, field, .. } => {
                (record.clone(), field.clone(), "undeclared")
            }
            Error::AssertionViolation { record, field, .. } => {
                (record.clone(), field.clone(), "assert")
            }
            Error::SchemaViolation { record, field, .. } => {
                (record.to_string(), field.to_string(), "type")
            }
            Error::PartitionMismatch { record, field, .. } => {
                (record.to_string(), field.to_string(), "partition")
            }
            _ => return None,
        };
        Some(Self {
            record,
            field,
            rule,
            detail: refusal.to_string(),
        })
    }
}
