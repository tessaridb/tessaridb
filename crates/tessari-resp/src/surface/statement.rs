//! A command as statements: their text, the values bound to them, and how
//! their outcomes become the command's reply.
//!
//! A command that is a handful of statements is built as text naming
//! parameters and a decoder for exactly those statements' outcomes. Outside a
//! `MULTI` its statements run as one script, inside one transaction when there
//! is more than one; inside a `MULTI` every queued command's statements run
//! together in ONE transaction, so `EXEC` is atomic. Each value is bound under
//! a name the shared [`Binder`] hands out, so two commands queued together
//! never bind the same name.

use tessaridb::{Outcome, Parameters, Value};

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::values;

/// Names handed to values and variables, unique within one script.
#[derive(Debug, Default)]
pub(crate) struct Binder {
    parameters: Parameters,
    next: usize,
}

impl Binder {
    /// Bind `value` and answer the name a statement uses for it.
    pub(crate) fn bind(&mut self, value: Value) -> String {
        let name = format!("p{}", self.next);
        self.next = self.next.saturating_add(1);
        let used = format!("${name}");
        self.parameters.insert(name, value);
        used
    }

    /// A variable name for a `LET` in the script.
    pub(crate) fn variable(&mut self) -> String {
        let name = format!("$v{}", self.next);
        self.next = self.next.saturating_add(1);
        name
    }

    /// `space:$key`, with the key bound.
    pub(crate) fn key(&mut self, space: &Space, key: &[u8]) -> String {
        let bound = self.bind(values::key(key));
        format!("{}:{bound}", space.name)
    }

    /// Everything bound.
    pub(crate) fn into_parameters(self) -> Parameters {
        self.parameters
    }
}

/// What turns a command's outcomes into its reply.
pub(crate) type Decode = Box<dyn FnOnce(Vec<Outcome>) -> Reply + Send>;

/// A command as statements.
pub(crate) struct Planned {
    /// Each statement, without its `;`.
    pub(crate) statements: Vec<String>,
    /// Given exactly one outcome per statement, in order.
    pub(crate) decode: Decode,
    /// The reply when the store refuses the statements as invalid, where the
    /// protocol names that refusal in its own words — `INCR` of a word is
    /// "not an integer", whatever the store calls it.
    pub(crate) on_invalid: Option<Reply>,
}

impl Planned {
    /// One statement.
    pub(crate) fn one(statement: String, decode: Decode) -> Self {
        Self::many(vec![statement], decode)
    }

    /// Several, run together.
    pub(crate) fn many(statements: Vec<String>, decode: Decode) -> Self {
        Self {
            statements,
            decode,
            on_invalid: None,
        }
    }

    /// Answer `reply` when the store refuses these statements as invalid.
    pub(crate) fn refusing_invalid_as(mut self, reply: Reply) -> Self {
        self.on_invalid = Some(reply);
        self
    }
}

/// The one value an outcome carries: a `GET`'s or a `RETURN`'s value, the
/// record a `RETURN BEFORE` handed back, `NONE` for an outcome holding none.
pub(crate) fn value_of(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Value(value) => value,
        Outcome::Records { records, .. } => records
            .into_iter()
            .next()
            .map_or(Value::None, |(_, value)| value),
        _ => Value::None,
    }
}

/// The values of an outcome holding an array, as a `RETURN [ … ]` answers.
pub(crate) fn values_of(outcome: Outcome) -> Vec<Value> {
    match value_of(outcome) {
        Value::Array(values) => values,
        _ => Vec::new(),
    }
}
