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

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A table with one unique field, holding one record.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
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
        .run(
            "BEGIN;\n\
             DELETE FROM holds WHERE object = 'tasks:7' LIMIT 1;\n\
             CREATE holds:2 = { object: 'tasks:7', by: 'second' };\n\
             COMMIT;",
        )
        .expect("the value the same transaction released is the transaction's to take");

    let after = session
        .run("SELECT by FROM holds WHERE object = 'tasks:7';")
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
        .run("CREATE holds:2 = { object: 'tasks:7', by: 'second' };")
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
        .run("CREATE holds:9 = { object: 'tasks:8', by: 'elsewhere' };")
        .unwrap();

    // A batch that deletes *something* must not be read as a batch that deleted
    // *this*. The check is per key, and this is what says so.
    let refusal = session
        .run(
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
