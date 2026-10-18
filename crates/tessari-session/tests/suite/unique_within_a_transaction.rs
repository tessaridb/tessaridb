//! A unique value moved from one record to another, inside one transaction.
//!
//! The index's uniqueness check reads **committed** state, which is what makes
//! it safe against a concurrent writer: the value cannot be claimed between the
//! read and the apply. What it could not see was the transaction's own removal —
//! so a batch that deleted the record holding a value and then wrote another
//! record with it was refused by the index it was maintaining, naming a record
//! the same batch was about to delete.
//!
//! That is the shape every silent index defect in this store has had, one level
//! up: a check that does not settle what this transaction has already done. It
//! surfaced from the agent-memory consumer, where a hold whose deadline had
//! passed is deleted and retaken in one transaction, and the retake was refused
//! by the hold it had just removed.
//!
//! The concurrency guarantee is unchanged and this pins that too: the
//! precondition is still the committed entry, so a second transaction that
//! claims the value first still wins and this one is refused.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// A table with one unique field, holding one record.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION holds;\n\
             DEFINE INDEX one_per_object ON holds FIELDS object UNIQUE;\n\
             CREATE holds:1 = { object: 'tasks:7', by: 'first' };",
        )
        .unwrap();
    session
}

#[test]
fn a_unique_value_is_freed_by_this_transactions_own_delete_before_it_is_taken_again() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now(
            "BEGIN;\n\
             DELETE FROM holds WHERE object = 'tasks:7' LIMIT 1;\n\
             CREATE holds:2 = { object: 'tasks:7', by: 'second' };\n\
             COMMIT;",
        )
        .expect("the value the same transaction released is the transaction's to take");

    let after = session
        .run_now("SELECT by FROM holds WHERE object = 'tasks:7';")
        .unwrap();
    let last = after.last().unwrap();
    assert!(
        format!("{last:?}").contains("second"),
        "the second holder is the one left, got {last:?}"
    );
}

#[test]
fn the_index_still_refuses_a_second_record_when_nothing_released_the_value() {
    let store = store();
    let mut session = ready(&store);

    // The half that must not move. Without a delete in the batch, the committed
    // entry is the answer and it still refuses — otherwise the fix above would
    // have removed the guarantee rather than corrected it.
    let refusal = session
        .run_now("CREATE holds:2 = { object: 'tasks:7', by: 'second' };")
        .unwrap_err()
        .to_string();

    assert!(
        refusal.contains("one_per_object"),
        "the refusal names the index that made it, got {refusal}"
    );
}

#[test]
fn a_delete_of_a_different_record_does_not_free_the_value() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE holds:9 = { object: 'tasks:8', by: 'elsewhere' };")
        .unwrap();

    // A batch that deletes *something* must not be read as a batch that deleted
    // *this*. The check is per key, and this is what says so.
    let refusal = session
        .run_now(
            "BEGIN;\n\
             DELETE FROM holds WHERE object = 'tasks:8' LIMIT 1;\n\
             CREATE holds:2 = { object: 'tasks:7', by: 'second' };\n\
             COMMIT;",
        )
        .unwrap_err()
        .to_string();

    assert!(
        refusal.contains("one_per_object"),
        "releasing another value frees nothing here, got {refusal}"
    );
}

// A log record's mutations are stored in address order, not in the order the
// statements ran. So a record whose id sorts BEFORE the holder's is maintained
// first, while the holder's release is still ahead of it — and its entry would
// then be deleted by that release, leaving the value claimable by anyone.

#[test]
fn a_lower_id_takes_the_value_a_higher_ids_delete_freed_in_the_same_transaction() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "CREATE holds:16 = { object: 'tasks:9', by: 'first' };\n\
             BEGIN;\n\
             DELETE holds:16;\n\
             CREATE holds:15 = { object: 'tasks:9', by: 'second' };\n\
             COMMIT;",
        )
        .expect("the value the same transaction released is the transaction's to take");

    let found = session
        .run_now("SELECT by FROM holds WHERE object = 'tasks:9' USING INDEX one_per_object;")
        .unwrap();
    let last = format!("{:?}", found.last().unwrap());
    assert!(
        last.contains("second"),
        "the index answers the new holder, got {last}"
    );

    // The entry survived the holder's release: a third record is still refused.
    let refusal = session
        .run_now("CREATE holds:3 = { object: 'tasks:9', by: 'third' };")
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("one_per_object"),
        "the value is held, so a third record is refused, got {refusal}"
    );
}

#[test]
fn two_records_exchange_their_unique_values_in_one_transaction() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "CREATE holds:2 = { object: 'tasks:8', by: 'second' };\n\
             BEGIN;\n\
             UPDATE holds:1 SET object = 'tasks:8';\n\
             UPDATE holds:2 SET object = 'tasks:7';\n\
             COMMIT;",
        )
        .expect("each value is released by the record that held it");

    for (object, by) in [("tasks:7", "second"), ("tasks:8", "first")] {
        let found = session
            .run_now(&format!(
                "SELECT by FROM holds WHERE object = '{object}' USING INDEX one_per_object;"
            ))
            .unwrap();
        let last = format!("{:?}", found.last().unwrap());
        assert!(last.contains(by), "{object} is held by {by}, got {last}");
    }
    let refusal = session
        .run_now("CREATE holds:3 = { object: 'tasks:8', by: 'third' };")
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("one_per_object"),
        "both entries survived the exchange, got {refusal}"
    );
}

#[test]
fn a_record_keeping_its_value_still_refuses_a_lower_id_in_the_same_transaction() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE holds:16 = { object: 'tasks:9', by: 'first' };")
        .unwrap();

    // The holder is rewritten in the same transaction but keeps the value, so
    // nothing was released and the lower id must still be refused.
    let refusal = session
        .run_now(
            "BEGIN;\n\
             UPDATE holds:16 SET by = 'still first';\n\
             CREATE holds:15 = { object: 'tasks:9', by: 'second' };\n\
             COMMIT;",
        )
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("one_per_object"),
        "a value its holder kept is not released, got {refusal}"
    );
}
