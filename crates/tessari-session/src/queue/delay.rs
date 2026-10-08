//! `RELEASE … NOT BEFORE`: work handed back for later in the same write
//! (ADR-0124 D6).

use tessari_ql::Span;
use tessari_storage::{QueueDeclaration, Transaction};
use tessari_types::{Datetime, Value};

use super::later;
use crate::error::{Error, Result};
use crate::session::Session;

/// The declared `NOT BEFORE` field and the instant to write into it, when the
/// release asked for one (ADR-0124 D6).
pub(super) fn delay_field(
    declared: &QueueDeclaration,
    not_before: Option<Datetime>,
    queue: &str,
    span: Span,
) -> Result<Option<(String, Datetime)>> {
    let Some(instant) = not_before else {
        return Ok(None);
    };
    let Some(field) = &declared.not_before else {
        return Err(Error::NoDelayField {
            queue: queue.to_owned(),
            span,
        });
    };
    Ok(Some((field.clone(), instant)))
}

impl Session<'_> {
    /// What a release's `NOT BEFORE` names: an instant as written, or a span
    /// added to this node's clock once, here, so every copy holds one instant.
    pub(crate) fn release_instant(
        &self,
        transaction: &mut Transaction<'_>,
        written: Option<&tessari_ql::Expr>,
        span: Span,
    ) -> Result<Option<Datetime>> {
        let Some(written) = written else {
            return Ok(None);
        };
        match self.evaluate(transaction, written)? {
            Value::Datetime(instant) => Ok(Some(instant)),
            Value::Duration(by) => Ok(Some(later(crate::call::instant(span)?, by, span)?)),
            other => Err(Error::NotAnInstant {
                found: other.type_name(),
                span,
            }),
        }
    }
}
