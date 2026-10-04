//! A snapshot restored on its own settles every transaction across leaders its
//! cut decided, and leaves the undecided for the log above (ADR-0112 D14f,
//! Q-922b).

use tessari_encoding::{
    Across, Decision, LogId, LogRecord, Mutation, Part, Participant, Provenance, RecordValue,
    StampedValue, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_storage::{Catalog, Reach, Store, TableShape};
use tessari_types::{RecordId, Sequence};

use crate::restore::store;

/// Two databases with a table each, and the address of `r` in each.
fn homes(source: &Store) -> [(Reach, tessari_storage::RecordAddress); 2] {
    let mut defining = source.begin().unwrap();
    let mut catalog = Catalog::new(&mut defining);
    let namespace = catalog.create_namespace("ns").unwrap().id;
    let mut homes = Vec::new();
    for name in ["one", "two"] {
        let database = catalog.create_database(namespace, name).unwrap().id;
        let table = catalog
            .create_table(namespace, database, "t", TableShape::default())
            .unwrap()
            .id;
        homes.push((
            Reach::Database(namespace, database),
            tessari_storage::RecordAddress::new(namespace, database, table, RecordId::from("r")),
        ));
    }
    defining.commit().unwrap();
    homes.try_into().unwrap()
}

/// Prepare `transaction`'s write of `r` in each home it names, in that home's
/// log at `at`, as a follower holding both applies them.
fn prepare(
    source: &Store,
    homes: &[(Reach, tessari_storage::RecordAddress)],
    transaction: TransactionId,
    at: u64,
) {
    let coordinator = homes[0].0;
    for (range, address) in homes {
        let write = Mutation {
            namespace: address.namespace,
            database: address.database,
            table: address.table,
            id: address.id.clone(),
            shard: None,
            value: StampedValue::new(RecordValue::Present(b"new".to_vec())).from_transaction(
                Provenance {
                    transaction,
                    provisional: true,
                    coordinator,
                    participants: Vec::new(),
                },
            ),
        };
        let record = LogRecord::new(vec![write]).across(Across {
            transaction,
            part: Part::Prepare { coordinator },
        });
        source
            .apply_record_in(LogId::line(*range), Sequence::new(at), &record)
            .unwrap();
    }
}

#[test]
fn a_restore_settles_what_its_cut_decided_and_nothing_else() {
    let (_, source) = store();
    let homes = homes(&source);
    let staged = TransactionId::new([1; TRANSACTION_ID_LEN]);
    let undecided = TransactionId::new([2; TRANSACTION_ID_LEN]);
    // Both parts landed and the record stages: committed implicitly.
    prepare(&source, &homes, staged, 1);
    let participants = homes
        .iter()
        .map(|(range, _)| Participant {
            range: *range,
            prepared_at: None,
        })
        .collect();
    let staging = LogRecord::new(Vec::new()).across(Across {
        transaction: staged,
        part: Part::Decide(TransactionRecord {
            decision: Decision::Staging,
            deadline: 0,
            participants,
        }),
    });
    source
        .apply_record_in(LogId::line(homes[0].0), Sequence::new(2), &staging)
        .unwrap();
    // One part of another, on a record of its own, and nothing decided.
    let other = [(
        homes[0].0,
        tessari_storage::RecordAddress {
            id: RecordId::from("s"),
            ..homes[0].1.clone()
        },
    )];
    prepare(&source, &other, undecided, 3);

    let mut file = Vec::new();
    tessari_backup::write_state(&source, &mut file).unwrap();
    let (_, restored) = store();
    tessari_backup::read_state(&restored, || Ok(file.as_slice())).unwrap();

    let standing: Vec<_> = restored
        .standing_across()
        .unwrap()
        .into_iter()
        .map(|(transaction, _)| transaction)
        .collect();
    assert_eq!(standing, [undecided]);
    assert_eq!(
        restored
            .transaction_record(staged)
            .unwrap()
            .map(|record| record.decision),
        Some(Decision::Committed)
    );
    let reading = restored.begin().unwrap();
    for (_, address) in &homes {
        assert_eq!(reading.get(address).unwrap().as_deref(), Some(&b"new"[..]));
    }
    assert_eq!(reading.get(&other[0].1).unwrap(), None);
}
