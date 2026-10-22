#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn outcomes_carried_back_read_as_the_answering_node_gave_them() {
    let given = vec![
        Outcome::Done,
        Outcome::Value(Value::from(7)),
        Outcome::Records {
            records: vec![(RecordId::from("a"), Value::from("x"))],
            plan: Plan::new(AccessPath::Record),
            notes: Vec::new(),
            suggestion: None,
            only: true,
        },
        Outcome::Keys(vec![RecordId::from("k"), RecordId::Int(3)]),
    ];
    let read = decode(&render(&Ok(given))).unwrap();
    assert_eq!(read.len(), 4);
    assert!(matches!(read[0], Outcome::Done));
    assert!(matches!(&read[1], Outcome::Value(value) if *value == Value::from(7)));
    assert!(matches!(
        &read[2],
        Outcome::Records { records, .. }
            if *records == vec![(RecordId::from("a"), Value::from("x"))]
    ));
    assert!(matches!(
        &read[3],
        Outcome::Keys(keys) if *keys == vec![RecordId::from("k"), RecordId::Int(3)]
    ));
}

#[test]
fn a_refusal_carried_back_keeps_its_class_and_its_words() {
    let refused: Result<Vec<Outcome>> = Err(crate::Error::NoOpenTransaction {
        span: tessari_ql::Span { start: 0, end: 0 },
    });
    let words = refused.as_ref().unwrap_err().to_string();
    let class = refused.as_ref().unwrap_err().class();
    let carried = decode(&render(&refused)).unwrap_err();
    assert_eq!(
        carried,
        CarriedRefusal {
            class,
            message: words
        }
    );
}
