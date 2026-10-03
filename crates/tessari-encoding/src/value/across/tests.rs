use tessari_kv::ErrorCategory;
use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach, RecordId, Sequence, ShardId, TableId};

use super::super::{
    FLAG_ACROSS, HEADER_LEN, LogRecord, Mutation, RecordValue, StampedValue, StoreValue,
};
use super::*;
use crate::error::{Error, Result};

const TRANSACTION: TransactionId = TransactionId::new([7; TRANSACTION_ID_LEN]);

fn coordinator() -> Reach {
    Reach::Namespace(NamespaceId::new(1))
}

fn intent(id: &str) -> Mutation {
    Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        id: RecordId::from(id),
        shard: None,
        value: StampedValue::new(RecordValue::Present(b"v".to_vec())),
    }
}

fn round_trip(record: &LogRecord) -> Result<LogRecord> {
    LogRecord::decode(record.encode().as_slice())
}

#[test]
fn every_part_round_trips_with_its_mutations_behind_it() -> Result<()> {
    let record_of = |decision| TransactionRecord {
        decision,
        deadline: 1_700_000_000_123,
        participants: vec![
            Participant {
                range: coordinator(),
                prepared_at: Some(Sequence::new(41)),
            },
            Participant {
                range: Reach::Shard(
                    NamespaceId::new(1),
                    DatabaseId::new(2),
                    TableId::new(3),
                    ShardId::new(4),
                ),
                prepared_at: None,
            },
        ],
    };
    let parts = [
        Part::Prepare {
            coordinator: coordinator(),
        },
        Part::Decide(record_of(Decision::Pending)),
        Part::Decide(record_of(Decision::Committed)),
        Part::Decide(record_of(Decision::Aborted)),
        Part::Resolve { committed: true },
        Part::Resolve { committed: false },
        Part::Forget {
            coordinator: coordinator(),
        },
    ];
    for part in parts {
        let across = Across {
            transaction: TRANSACTION,
            part,
        };
        let mut record =
            LogRecord::at(Epoch::new(9), vec![intent("a"), intent("b")]).across(across.clone());
        record.set_order(Sequence::new(12));
        let decoded = round_trip(&record)?;
        assert_eq!(decoded, record);
        assert_eq!(decoded.part_of(), Some(&across));
        assert_eq!(
            decoded.mutations().len(),
            2,
            "the mutations follow the section"
        );
        // The fixed offsets the section must not move.
        let bytes = record.encode();
        assert_eq!(LogRecord::epoch_in(bytes.as_slice())?, Epoch::new(9));
        assert_eq!(
            LogRecord::order_in(bytes.as_slice())?,
            Some(Sequence::new(12))
        );
    }
    Ok(())
}

#[test]
fn a_record_outside_any_such_transaction_says_so() -> Result<()> {
    let record = LogRecord::new(vec![intent("a")]);
    let bytes = record.encode();
    assert_eq!(bytes.as_slice()[1] & FLAG_ACROSS, 0, "the bit stays clear");
    assert_eq!(round_trip(&record)?.part_of(), None);
    Ok(())
}

#[test]
fn a_version_keeps_the_transaction_it_was_resolved_from() -> Result<()> {
    // An intent knows only its own range; a resolved version carries every
    // participant and where its prepare landed, so a reader can decide without
    // the record (ADR-0112 D6a).
    versions_keep(&Provenance {
        transaction: TRANSACTION,
        provisional: true,
        coordinator: coordinator(),
        participants: Vec::new(),
    })?;
    versions_keep(&Provenance {
        transaction: TRANSACTION,
        provisional: false,
        coordinator: coordinator(),
        participants: vec![
            Participant {
                range: coordinator(),
                prepared_at: Some(Sequence::new(7)),
            },
            Participant {
                range: Reach::Store,
                prepared_at: None,
            },
        ],
    })?;
    Ok(())
}

fn versions_keep(provenance: &Provenance) -> Result<()> {
    let shapes = [
        StampedValue::new(RecordValue::Present(b"payload".to_vec())),
        StampedValue::new(RecordValue::Present(b"payload".to_vec())).expiring(99),
        StampedValue::new(RecordValue::Tombstone),
    ];
    for shape in shapes {
        let resolved = shape.clone().from_transaction(provenance.clone());
        let decoded = StampedValue::decode(resolved.encode().as_slice())?;
        assert_eq!(decoded, resolved);
        assert_eq!(decoded.provenance(), Some(provenance));
        assert_eq!(decoded.expires(), shape.expires());
        assert_eq!(decoded.value(), shape.value());
        // And without it, the version is the one it always was.
        let plain = StampedValue::decode(shape.encode().as_slice())?;
        assert_eq!(plain.provenance(), None);
        assert_eq!(shape.encode().as_slice()[1] & FLAG_ACROSS, 0);
    }
    Ok(())
}

#[test]
fn a_part_this_build_does_not_know_is_refused_as_incompatible() -> Result<()> {
    let record = LogRecord::new(vec![]).across(Across {
        transaction: TRANSACTION,
        part: Part::Resolve { committed: true },
    });
    let mut bytes = record.encode().as_slice().to_vec();
    // Header, then the sixteen bytes of the id, then the part byte.
    let part_at = HEADER_LEN + TRANSACTION_ID_LEN;
    bytes[part_at] = 0x7f;
    let Err(refused) = LogRecord::decode(&bytes) else {
        return Err(Error::ReservedFlags { flags: 0 });
    };
    assert!(
        matches!(
            refused,
            Error::UnknownAcross {
                what: "part",
                found: 0x7f
            }
        ),
        "{refused:?}"
    );
    assert_eq!(refused.category(), ErrorCategory::Incompatible);
    Ok(())
}

#[test]
fn a_cut_short_section_is_an_error_and_not_a_panic() {
    let record = LogRecord::new(vec![]).across(Across {
        transaction: TRANSACTION,
        part: Part::Decide(TransactionRecord {
            decision: Decision::Pending,
            deadline: 5,
            participants: vec![Participant {
                range: coordinator(),
                prepared_at: Some(Sequence::new(1)),
            }],
        }),
    });
    let bytes = record.encode().as_slice().to_vec();
    for len in HEADER_LEN..bytes.len() {
        assert!(
            LogRecord::decode(&bytes[..len]).is_err(),
            "decoded {len} bytes"
        );
    }
}
