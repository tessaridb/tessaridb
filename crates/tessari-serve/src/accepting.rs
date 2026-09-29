//! What an accept loop does with an accept that failed (Q-834).
//!
//! Some failures clear on their own — out of file descriptors until a
//! connection closes, a connection reset before it was accepted — and a loop
//! retrying them at once turns one into a spinning core. Everything else means
//! the listener itself has gone bad, and a node that goes on retrying it looks
//! alive to its supervisor while admitting nobody. Both surfaces and the peer
//! door decide by this one rule.

use std::io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, WouldBlock};
use std::time::Duration;

/// How long an accept loop rests after a failure it expects to pass.
///
/// Short, because while it rests nobody new is admitted.
pub const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// EMFILE and ENFILE: this process, or the whole system, is out of file
/// descriptors until a connection closes.
const OUT_OF_DESCRIPTORS: [i32; 2] = [24, 23];

/// Whether an accept failure is one that passes on its own.
#[must_use]
pub fn passes(failure: &std::io::Error) -> bool {
    matches!(
        failure.kind(),
        ConnectionAborted | ConnectionReset | Interrupted | WouldBlock
    ) || failure
        .raw_os_error()
        .is_some_and(|code| OUT_OF_DESCRIPTORS.contains(&code))
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::passes;

    #[test]
    fn what_clears_on_its_own_is_retried_and_a_listener_gone_bad_is_not() {
        for (failure, retried) in [
            (Error::from(ErrorKind::ConnectionAborted), true),
            (Error::from(ErrorKind::ConnectionReset), true),
            (Error::from(ErrorKind::Interrupted), true),
            (Error::from(ErrorKind::WouldBlock), true),
            (Error::from_raw_os_error(24), true),
            (Error::from_raw_os_error(23), true),
            // EBADF: the descriptor is gone and every retry answers the same.
            (Error::from_raw_os_error(9), false),
            (Error::from(ErrorKind::InvalidInput), false),
            (Error::other("anything else"), false),
        ] {
            assert_eq!(passes(&failure), retried, "{failure}");
        }
    }
}
