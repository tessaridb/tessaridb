//! Tests for the shipment: the envelope's wire shape, the ask and answer
//! bodies, and the token's environment-name rule.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use tessari_encoding::{LogId, LogRecord, Mutation, NODE_ID_LEN, RecordValue, StampedValue};
use tessari_storage::{Envelope, Reach};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use super::{LinkAnswer, LinkAsk, token_variable};
use crate::frame;

fn record(at: u8) -> LogRecord {
    LogRecord::new(vec![Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        id: RecordId::from("orders:1"),
        shard: None,
        value: StampedValue::new(RecordValue::Present(vec![at])),
    }])
}

fn envelope() -> Envelope {
    Envelope {
        origin: [7_u8; NODE_ID_LEN],
        log: LogId::line(Reach::Database(NamespaceId::new(1), DatabaseId::new(2))),
        from: Sequence::new(41),
        records: vec![
            (Sequence::new(41), record(1)),
            (Sequence::new(42), record(2)),
        ],
    }
}

#[test]
fn an_envelope_round_trips_through_the_wire_shape() {
    let held = envelope();
    let read = super::decode_envelope(&super::encode_envelope(&held)).expect("a round trip");

    assert_eq!(read, held);
    assert_eq!(read.origin, [7_u8; NODE_ID_LEN]);
    assert_eq!(read.from, Sequence::new(41));
    assert_eq!(read.records.len(), 2);
    assert_eq!(read.records[1].0, Sequence::new(42));
}

#[test]
fn an_ask_carries_the_token_the_remote_spelling_and_the_envelope() {
    let held = LinkAsk {
        token: [9_u8; 32],
        namespace: "prod".to_owned(),
        database: "warehouse".to_owned(),
        envelope: envelope(),
    };
    let read = LinkAsk::decode(&held.encode()).expect("a round trip");

    assert_eq!(read.token, [9_u8; 32]);
    assert_eq!(read.namespace, "prod");
    assert_eq!(read.database, "warehouse");
    assert_eq!(read.envelope, held.envelope);
}

#[test]
fn an_answer_is_the_confirmed_position_alone() {
    let held = LinkAnswer {
        confirmed: Sequence::new(8128),
    };
    let read = LinkAnswer::decode(&held.encode()).expect("a round trip");

    assert_eq!(read.confirmed, Sequence::new(8128));
}

#[test]
fn a_body_that_stops_early_is_malformed_not_truncated_silently() {
    let held = envelope();
    let mut body = super::encode_envelope(&held);
    body.pop();
    let refused = super::decode_envelope(&body).expect_err("a short body refuses");

    assert!(
        matches!(refused, crate::error::Error::Malformed),
        "expected the malformed refusal, got {refused}"
    );
}

#[test]
fn the_link_kind_is_tagged_beside_the_part_it_follows() {
    assert_eq!(frame::Kind::Link.tag(), 40);
    assert_eq!(frame::Kind::from_tag(40), Some(frame::Kind::Link));
    // The peer link's space is untouched — 39 is its StateEntries.
    assert_eq!(frame::Kind::from_tag(39), None);
}

#[test]
fn the_token_variable_names_the_link_it_serves() {
    assert_eq!(token_variable("east"), "TESSARIDB_LINK_TOKEN_EAST");
    assert_eq!(
        token_variable("prod-dr_2"),
        "TESSARIDB_LINK_TOKEN_PROD_DR_2"
    );
    // Reach::Database is what the ask names; the test pins the spelling shape.
    let reach = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    assert!(matches!(reach, Reach::Database(_, _)));
}
