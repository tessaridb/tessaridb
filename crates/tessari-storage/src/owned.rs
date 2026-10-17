//! State one task owns, shareable so a reference to it may cross an `.await`.
//!
//! A transaction or a graph walk keeps memo tables and sets that only its own
//! task reads and writes. They were `RefCell`s, which are not `Sync`, so every
//! future holding a reference to their owner was not `Send` and could not run
//! as a task of a multi-thread runtime. [`Owned`] keeps the same two calls over
//! a mutex instead.
//!
//! The mutex is never contended: one task holds the owner, so a lock always
//! finds it free. What it adds is `Sync`, and the rule that comes with it: a
//! guard ends before the next `.await` (a guard held across one makes the future
//! not `Send`, which fails to compile where the future is spawned), and a field
//! is never borrowed twice at once (a `RefCell` would panic there; a mutex would
//! wait on itself).

use std::sync::{Mutex, MutexGuard, PoisonError};

/// A value one task owns, behind an uncontended lock.
#[derive(Debug, Default)]
pub(crate) struct Owned<T>(Mutex<T>);

impl<T> Owned<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    /// Read the value. Ends before the next `.await`.
    pub(crate) fn borrow(&self) -> MutexGuard<'_, T> {
        // A panic while the owner held the guard poisons it; the owner is the
        // only one that could read it, and the panic already ended its work.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Change the value. Ends before the next `.await`.
    pub(crate) fn borrow_mut(&self) -> MutexGuard<'_, T> {
        self.borrow()
    }

    /// Change the value through the owner's exclusive reference, taking no lock.
    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}
