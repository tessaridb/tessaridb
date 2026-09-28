//! Reading a function's arguments, and refusing the wrong kind by name.

use crate::error::{Error, Result};
use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

pub(crate) fn text_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&str> {
    match arguments.get(at) {
        Some(Value::String(text)) => Ok(text),
        other => Err(wrong_type(function, at, "a string", named(other), span)),
    }
}

pub(crate) fn bytes_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&[u8]> {
    match arguments.get(at) {
        Some(Value::Bytes(held)) => Ok(held),
        other => Err(wrong_type(function, at, "bytes", named(other), span)),
    }
}

pub(crate) fn array_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&[Value]> {
    match arguments.get(at) {
        Some(Value::Array(items)) => Ok(items),
        other => Err(wrong_type(function, at, "an array", named(other), span)),
    }
}

pub(crate) fn object_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&std::collections::BTreeMap<String, Value>> {
    match arguments.get(at) {
        Some(Value::Object(fields)) => Ok(fields),
        other => Err(wrong_type(function, at, "an object", named(other), span)),
    }
}

/// An argument that has to be a whole number, for a position or a count.
///
/// A fraction is refused rather than truncated, on the rule the casts follow:
/// `math::round` already says which whole number was meant.
pub(crate) fn whole(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<i64> {
    let number = number_at(function, arguments, at, span)?;
    number.as_exact_integer().ok_or(Error::CallFailed {
        function,
        reason: "a position is a whole number; math::round says which one was meant",
        span,
    })
}

pub(crate) fn datetime_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&tessari_types::Datetime> {
    match arguments.get(at) {
        Some(Value::Datetime(held)) => Ok(held),
        other => Err(wrong_type(function, at, "a datetime", named(other), span)),
    }
}

pub(crate) fn duration_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&tessari_types::Duration> {
    match arguments.get(at) {
        Some(Value::Duration(held)) => Ok(held),
        other => Err(wrong_type(function, at, "a duration", named(other), span)),
    }
}

pub(crate) fn number_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&Number> {
    match arguments.get(at) {
        Some(Value::Number(number)) => Ok(number),
        other => Err(wrong_type(function, at, "a number", named(other), span)),
    }
}

pub(crate) fn named(value: Option<&Value>) -> &'static str {
    value.map_or("nothing", Value::type_name)
}

pub(crate) fn wrong_type(
    function: Function,
    at: usize,
    expected: &'static str,
    found: &'static str,
    span: Span,
) -> Error {
    Error::WrongArgument {
        function,
        at: at.saturating_add(1),
        expected,
        found,
        span,
    }
}
