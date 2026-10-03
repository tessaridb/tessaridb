// Test assertions are exactly where a panic is the correct outcome.
#![allow(clippy::panic, clippy::unwrap_used)]

use std::cell::Cell;

use super::WalSyncs;
use tessari_kv::Error;

/// A flush that counts itself and succeeds.
fn counting(flushes: &Cell<u32>) -> impl FnOnce() -> Result<(), Error> + '_ {
    move || {
        flushes.set(flushes.get().saturating_add(1));
        Ok(())
    }
}

#[test]
fn a_round_with_nothing_unsynced_pays_no_flush() {
    let syncs = WalSyncs::default();
    let flushes = Cell::new(0);
    syncs
        .sync_through(syncs.landed_so_far(), counting(&flushes))
        .unwrap();
    assert_eq!(flushes.get(), 0);
}

#[test]
fn a_round_after_unsynced_writes_flushes_once_and_then_is_covered() {
    let syncs = WalSyncs::default();
    let before = syncs.landed_so_far();
    syncs.landed(before, false);
    syncs.landed(syncs.landed_so_far(), false);
    let flushes = Cell::new(0);
    syncs
        .sync_through(syncs.landed_so_far(), counting(&flushes))
        .unwrap();
    assert_eq!(flushes.get(), 1);
    syncs
        .sync_through(syncs.landed_so_far(), counting(&flushes))
        .unwrap();
    assert_eq!(
        flushes.get(),
        1,
        "a second round with nothing new is covered"
    );
}

#[test]
fn a_synced_write_covers_what_landed_before_it_began_and_nothing_after() {
    let syncs = WalSyncs::default();
    // An unsynced apply, then a synced commit that began after it landed.
    syncs.landed(syncs.landed_so_far(), false);
    let unsynced = syncs.landed_so_far();
    syncs.landed(syncs.landed_so_far(), true);
    let flushes = Cell::new(0);
    syncs.sync_through(unsynced, counting(&flushes)).unwrap();
    assert_eq!(flushes.get(), 0, "the synced write's own sync covered it");
    // An unsynced apply after the synced write began is not covered by it.
    syncs.landed(syncs.landed_so_far(), false);
    syncs
        .sync_through(syncs.landed_so_far(), counting(&flushes))
        .unwrap();
    assert_eq!(flushes.get(), 1);
}

#[test]
fn a_failed_flush_fails_every_later_round_without_flushing_again() {
    let syncs = WalSyncs::default();
    syncs.landed(syncs.landed_so_far(), false);
    let failed = syncs.sync_through(syncs.landed_so_far(), || {
        Err(Error::Backend {
            backend: "test",
            reason: "the device refused the sync".to_owned(),
            source: None,
        })
    });
    assert!(failed.is_err());
    // After a failed sync the pages may be gone; nothing landed since may be
    // called durable, and asking the device again would only say yes.
    let flushes = Cell::new(0);
    let later = syncs.sync_through(syncs.landed_so_far(), counting(&flushes));
    assert!(later.is_err(), "{later:?}");
    assert_eq!(flushes.get(), 0);
}
