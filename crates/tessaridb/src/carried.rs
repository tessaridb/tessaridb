//! Outcomes carried back from the node that answered a request, for a surface
//! that makes its own replies from them — RESP and Kafka (G079 W5d).
//!
//! The wire and HTTP carry their own bytes back: the answering node renders the
//! caller's surface and this node forwards them. RESP and Kafka build a reply
//! from the OUTCOMES, so the answering node sends those instead, reduced to the
//! three shapes these surfaces read — a value, records, keys — in the store's
//! own value codec, or a refusal's class and the store's own words.

use std::collections::BTreeMap;

use tessari_types::{RecordId, RecordRef, RefusalClass, TableId, Value};

use crate::{AccessPath, Coordinated, Outcome, Plan, Result};

/// The kind an answered request is carried back under; a refusal's kind is its
/// class byte, which is never zero.
const ANSWERED: u16 = 0;

/// A refusal the answering node gave, in its class and its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedRefusal {
    /// What the caller should do about it.
    pub class: RefusalClass,
    /// The store's own sentence.
    pub message: String,
}

/// What the answering node sends back for `ran`.
#[must_use]
pub fn render(ran: &Result<Vec<Outcome>>) -> Coordinated {
    match ran {
        Ok(outcomes) => Coordinated {
            kind: ANSWERED,
            body: tessari_encoding::encode_payload(&Value::Array(
                outcomes.iter().map(shape_of).collect(),
            ))
            .as_slice()
            .to_vec(),
        },
        Err(refused) => Coordinated {
            kind: u16::from(refused.class().byte()),
            body: refused.to_string().into_bytes(),
        },
    }
}

/// The outcomes, or the refusal, `answer` carries.
///
/// # Errors
///
/// The answering node's refusal; an answer that does not decode is an
/// [`RefusalClass::Internal`] refusal naming why.
pub fn decode(answer: &Coordinated) -> std::result::Result<Vec<Outcome>, CarriedRefusal> {
    if answer.kind != ANSWERED {
        let class = u8::try_from(answer.kind)
            .ok()
            .and_then(RefusalClass::from_byte)
            .unwrap_or(RefusalClass::Internal);
        return Err(CarriedRefusal {
            class,
            message: String::from_utf8_lossy(&answer.body).into_owned(),
        });
    }
    let unreadable = |why: &str| CarriedRefusal {
        class: RefusalClass::Internal,
        message: format!("the answering node's outcomes could not be read: {why}"),
    };
    let Value::Array(shapes) = tessari_encoding::decode_payload(&answer.body)
        .map_err(|why| unreadable(&why.to_string()))?
    else {
        return Err(unreadable("not a list"));
    };
    shapes
        .into_iter()
        .map(|shape| outcome_of(shape).ok_or_else(|| unreadable("an unknown shape")))
        .collect()
}

fn shape_of(outcome: &Outcome) -> Value {
    let (name, value) = match outcome {
        Outcome::Value(value) => ("value", value.clone()),
        Outcome::Records { records, .. } => (
            "records",
            Value::Array(
                records
                    .iter()
                    .map(|(id, value)| Value::Array(vec![as_value(id), value.clone()]))
                    .collect(),
            ),
        ),
        Outcome::Keys(keys) => ("keys", Value::Array(keys.iter().map(as_value).collect())),
        _ => return Value::Object(BTreeMap::new()),
    };
    Value::Object(BTreeMap::from([(name.to_owned(), value)]))
}

fn outcome_of(shape: Value) -> Option<Outcome> {
    let Value::Object(mut fields) = shape else {
        return None;
    };
    let Some((name, value)) = fields.pop_first() else {
        return Some(Outcome::Done);
    };
    match (name.as_str(), value) {
        ("value", value) => Some(Outcome::Value(value)),
        ("records", Value::Array(pairs)) => Some(Outcome::Records {
            records: pairs
                .into_iter()
                .map(|pair| match pair {
                    Value::Array(pair) => match <[Value; 2]>::try_from(pair) {
                        Ok([id, value]) => Some((as_id(id)?, value)),
                        Err(_) => None,
                    },
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?,
            plan: Plan::new(AccessPath::Scan),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        }),
        ("keys", Value::Array(keys)) => Some(Outcome::Keys(
            keys.into_iter().map(as_id).collect::<Option<Vec<_>>>()?,
        )),
        _ => None,
    }
}

/// An identity as a value the codec carries: a reference whose table no
/// reader looks at.
fn as_value(id: &RecordId) -> Value {
    Value::Record(RecordRef {
        table: TableId::new(0),
        id: id.clone(),
    })
}

fn as_id(value: Value) -> Option<RecordId> {
    match value {
        Value::Record(reference) => Some(reference.id),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
