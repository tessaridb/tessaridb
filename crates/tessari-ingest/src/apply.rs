//! Turning a message into the record it was declared to become.
//!
//! # A field nobody named does not land
//!
//! The mapping is the whole schema. A message field the declaration does not
//! name is **dropped**, which is the anti-inference rule stated positively: a
//! producer adding a field changes nothing here, where an inferred mapping would
//! quietly start writing it.
//!
//! # The identity is what makes a replay converge
//!
//! Every record carries an id taken from a declared message field, and the write
//! is a replace at that id. So the same message applied twice produces one
//! record, which is exactly what makes at-least-once delivery usable — and it is
//! why `IDENTITY` is a required clause rather than a convenience.
//!
//! What that does **not** buy is exactly-once. Two messages with the same
//! identity and different contents still land as one record holding the later
//! one, and a redelivery after a crash re-applies a write that already happened.
//! Both are fine; neither is exactly-once, and this crate says so.

use tessari_storage::ConsumerDefinition;
use tessari_types::{Number, Path, RecordId, Value};

use crate::json;

/// A message, ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shaped {
    /// The record's identity, from the declared identity field.
    pub id: RecordId,
    /// The record itself: only the fields the mapping named.
    pub record: Value,
}

/// Read a message and shape it into a record.
///
/// # Errors
///
/// Returns the reason as text, which is what goes to the operator — either in a
/// quarantine record beside the payload, or as the reason a `stop` consumer
/// halted. It is prose rather than a type because the operator is the only
/// consumer of it and every branch below is a different sentence.
pub fn shape(payload: &[u8], definition: &ConsumerDefinition) -> Result<Shaped, String> {
    let message =
        json::read(payload).map_err(|failure| format!("the payload is not JSON: {failure}"))?;

    let Some(route) = Path::parse(&definition.identity) else {
        return Err(format!(
            "the identity field {:?} is not a route this store can follow",
            definition.identity
        ));
    };
    let Some(found) = route.resolve(&message) else {
        // Not an optional field. A message with no identity cannot be applied
        // idempotently, so applying it anyway would silently give up the one
        // property the delivery guarantee rests on.
        return Err(format!(
            "the message has no {:?}, which is the field the identity is taken from",
            definition.identity
        ));
    };
    let id = identity(found).ok_or_else(|| {
        format!(
            "{:?} holds {}, and a record identity is a whole number or a string",
            definition.identity,
            found.type_name()
        )
    })?;

    let mut fields = std::collections::BTreeMap::new();
    for pair in &definition.mapping {
        let Some(route) = Path::parse(&pair.from) else {
            return Err(format!(
                "{:?} is not a route this store can follow",
                pair.from
            ));
        };
        // A mapped field the message does not carry is left **absent** rather
        // than written as null. The two are different values in this store, and
        // absent is the true one: the producer did not send it.
        if let Some(held) = route.resolve(&message) {
            fields.insert(pair.to.clone(), held.clone());
        }
    }
    Ok(Shaped {
        id,
        record: Value::Object(fields),
    })
}

/// A record identity, if this value can be one.
///
/// Whole numbers and strings, which is what a record id is. A float is refused
/// rather than truncated: `1.0` and `1` would become the same record and `1.5`
/// would become `1`, so two different orders would collide under an id nobody
/// wrote.
fn identity(value: &Value) -> Option<RecordId> {
    match value {
        Value::Number(Number::Integer(held)) => Some(RecordId::Int(*held)),
        Value::String(held) if !held.is_empty() => Some(RecordId::Text(held.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use tessari_storage::{Mapped, OnFailure};
    use tessari_types::{DatabaseId, NamespaceId, TableId};

    fn declared(identity: &str, mapping: &[(&str, &str)]) -> ConsumerDefinition {
        ConsumerDefinition {
            id: 1,
            name: "orders_in".to_owned(),
            brokers: vec!["b:9092".to_owned()],
            topic: "orders".to_owned(),
            group: "g".to_owned(),
            format: "json".to_owned(),
            identity: identity.to_owned(),
            mapping: mapping
                .iter()
                .map(|(from, to)| Mapped {
                    from: (*from).to_owned(),
                    to: (*to).to_owned(),
                })
                .collect(),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            destination: TableId::new(1),
            on_failure: OnFailure::Quarantine,
            parallelism: 1,
            // Shaping a message is the half that has no identity in it: these
            // tests turn a payload into fields and never reach the store.
            declarer: None,
        }
    }

    fn field<'a>(shaped: &'a Shaped, name: &str) -> Option<&'a Value> {
        let Value::Object(fields) = &shaped.record else {
            panic!("not an object");
        };
        fields.get(name)
    }

    #[test]
    fn the_named_fields_land_and_nothing_else_does() {
        // The anti-inference rule, stated as a test: `secret` is in the payload
        // and is not in the mapping, so it does not reach the store. A producer
        // adding a field is a non-event.
        let definition = declared("order_id", &[("amount", "total")]);
        let shaped = shape(
            br#"{"order_id": 7, "amount": 500, "secret": "do not store me"}"#,
            &definition,
        )
        .unwrap();
        assert_eq!(shaped.id, RecordId::Int(7));
        assert_eq!(
            field(&shaped, "total"),
            Some(&Value::Number(Number::Integer(500)))
        );
        assert_eq!(field(&shaped, "secret"), None);
        assert_eq!(
            field(&shaped, "amount"),
            None,
            "the message's own name landed"
        );
    }

    #[test]
    fn a_nested_field_is_read_and_written_flat() {
        // Nested read, flat write — the asymmetry that keeps the mapping clause
        // from becoming a transformation language.
        let definition = declared("id", &[("placed.at", "placed_at")]);
        let shaped = shape(br#"{"id":"a1","placed":{"at":"2026-08-27"}}"#, &definition).unwrap();
        assert_eq!(shaped.id, RecordId::Text("a1".to_owned()));
        assert_eq!(
            field(&shaped, "placed_at"),
            Some(&Value::from("2026-08-27"))
        );
    }

    #[test]
    fn a_mapped_field_the_message_lacks_is_absent_rather_than_null() {
        // The two are different values in this store, and absent is the true
        // one: the producer did not send it. Writing null would be this crate
        // inventing a value.
        let definition = declared("id", &[("amount", "total"), ("note", "note")]);
        let shaped = shape(br#"{"id":1,"amount":5}"#, &definition).unwrap();
        assert_eq!(
            field(&shaped, "total"),
            Some(&Value::Number(Number::Integer(5)))
        );
        assert_eq!(field(&shaped, "note"), None);
    }

    #[test]
    fn a_message_with_no_identity_is_refused() {
        // Applying it anyway would give up idempotence silently, which is the
        // one property the delivery guarantee rests on.
        let definition = declared("order_id", &[("amount", "total")]);
        let failure = shape(br#"{"amount":5}"#, &definition).expect_err("shaped anyway");
        assert!(failure.contains("order_id"), "{failure}");
        assert!(failure.contains("identity"), "{failure}");
    }

    #[test]
    fn a_float_identity_is_refused_rather_than_truncated() {
        // `1.0` and `1` would become one record, and `1.5` would become `1`, so
        // two different orders would collide under an id nobody wrote.
        let definition = declared("order_id", &[("amount", "total")]);
        let failure = shape(br#"{"order_id":1.5,"amount":5}"#, &definition).expect_err("shaped");
        assert!(failure.contains("whole number or a string"), "{failure}");
    }

    #[test]
    fn an_empty_string_identity_is_refused() {
        // A record whose id is the empty string is one every message with a
        // missing field would converge onto — the opposite of idempotent.
        let definition = declared("order_id", &[("amount", "total")]);
        assert!(shape(br#"{"order_id":"","amount":5}"#, &definition).is_err());
    }

    #[test]
    fn a_payload_that_is_not_json_says_so_and_says_where() {
        let definition = declared("order_id", &[("amount", "total")]);
        let failure = shape(b"{not json", &definition).expect_err("shaped");
        assert!(failure.contains("not JSON"), "{failure}");
        assert!(
            failure.contains("byte"),
            "the reason does not say where: {failure}"
        );
    }

    #[test]
    fn the_same_message_shapes_to_the_same_record_every_time() {
        // The property the whole delivery claim rests on, asserted directly
        // rather than inferred from the write path: a replay converges because
        // the identity and the fields are a function of the payload alone.
        let definition = declared("order_id", &[("amount", "total")]);
        let payload = br#"{"order_id":7,"amount":500}"#;
        assert_eq!(
            shape(payload, &definition).unwrap(),
            shape(payload, &definition).unwrap()
        );
    }
}
