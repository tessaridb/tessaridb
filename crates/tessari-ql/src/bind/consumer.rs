//! A consumer or group name bound from a parameter (ADR-0127).
//!
//! The name is a quoted string — the client's own data, not a catalog object —
//! so a parameter may stand where it stands. Binding writes the string in, so
//! execution sees exactly what the literal would have given it.

use super::Binding;
use super::nested::slot_value;
use crate::ast::StatementKind;
use crate::error::{Error, Result};
use tessari_types::Value;

/// The consumer or group name a statement carries, bound from its parameter
/// when one was written in its place (ADR-0127). Every other value the
/// statement holds is bound by the caller's own match.
pub(super) fn bind_consumer_names(kind: &mut StatementKind, binding: &Binding<'_>) -> Result<()> {
    match kind {
        StatementKind::Use {
            consumer: Some(name),
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::Release {
            consumer: Some(name),
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::ReleaseAll {
            consumer: Some(name),
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::ReadTopic {
            consumer: Some(name),
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::AckTopic {
            consumer: name,
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::NackTopic {
            consumer: name,
            consumer_parameter: parameter,
            ..
        }
        | StatementKind::DefineGroup {
            name,
            name_parameter: parameter,
            ..
        }
        | StatementKind::DropGroup {
            name,
            name_parameter: parameter,
            ..
        }
        | StatementKind::AlterGroup {
            name,
            name_parameter: parameter,
            ..
        } => bind_consumer_name(parameter, name, binding),
        _ => Ok(()),
    }
}

/// The string a consumer or group name parameter is bound to, written where
/// the quoted literal would have put it (ADR-0127). Left standing for a `LET`
/// that has not run.
fn bind_consumer_name(
    parameter: &mut Option<crate::ast::ParameterSlot>,
    name: &mut String,
    binding: &Binding<'_>,
) -> Result<()> {
    let Some((written, value)) = slot_value(parameter.as_ref(), binding)? else {
        return Ok(());
    };
    match value {
        Value::String(held) if !held.is_empty() => {
            name.clone_from(held);
            *parameter = None;
            Ok(())
        }
        other => Err(Error::NotAConsumerName {
            name: written.name.clone(),
            found: match other {
                Value::String(_) => "an empty string".to_owned(),
                other => format!("a {}", other.type_name()),
            },
            span: written.span,
        }),
    }
}
