//! Signing in once and carrying the result.
//!
//! # The problem this exists for
//!
//! Checking a password is expensive by design: nineteen mebibytes and tens of
//! milliseconds, which is why this process runs at most
//! [`MAX_SIGN_IN_VERIFICATIONS`](tessari_constants::MAX_SIGN_IN_VERIFICATIONS)
//! of them at once. A surface where a connection *is* a session pays that once
//! and is done — the wire protocol works that way. HTTP does not have a
//! conversation to hang an identity on, so without something here it re-proves
//! the same password on every request, and a store's whole request rate is
//! bounded by a number chosen to bound an attacker.
//!
//! A [`Ticket`] is what a session hands out instead: proof that a sign-in
//! already happened, and of exactly whom.
//!
//! # Why the ticket is the user record
//!
//! Taking a ticket up re-reads that user from the catalog and requires the
//! record to be **unchanged**. That single comparison is what makes a token
//! safe to hold, because it revokes on every event that ought to revoke:
//!
//! | what changed | what happens to the ticket |
//! |---|---|
//! | the password | the stored hash differs — dead |
//! | the role | the role differs — dead |
//! | the tenancy | the namespace or database differs — dead |
//! | the user is dropped | there is no record to compare — dead |
//!
//! The alternative — a revocation list — is a cascade that every future
//! statement touching a user has to remember to call. `ALTER USER` was added
//! the day before this file and already moves two fields; the statement after
//! it would have been the one that silently did not revoke. A comparison has
//! nothing to forget, and that is the entire argument for it.
//!
//! # Why grants are deliberately *not* in it
//!
//! [`within_grants`](crate::session::Session) reads grants from the catalog per
//! statement, so a `GRANT` or `REVOKE` already narrows a ticket-borne session on
//! its very next statement. Folding them into the ticket would make a narrowing
//! take effect *later* than it does today, which is the wrong direction for the
//! one part of the permission system whose job is to take authority away.
//!
//! # What a ticket is not
//!
//! It is a bearer credential: whoever holds the text is the user until the
//! record changes. It travels in a header exactly as a password does, over a
//! connection this node does not encrypt, and that is stated in the operations
//! notes rather than implied here. What it is *better* at than a password is
//! that it can be thrown away and it dies on its own — a password can do
//! neither.
//!
//! Taking one up asks the throttle nothing, because it verifies nothing. A
//! stale ticket is not a guess at a password; it is a legitimate token whose
//! identity moved, and counting it against the name would let a rotation lock
//! its own user out.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use tessari_storage::{Catalog, UserDefinition};

use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::session::Session;

/// How many random bytes a token carries.
///
/// Two hundred and fifty-six bits, so that guessing one is not a strategy at any
/// request rate a network permits. It is written out as hexadecimal, which
/// doubles the length and keeps the text safe in a header, a log line and a URL
/// without an escaping rule anybody has to remember.
const TOKEN_BYTES: usize = 32;

/// Proof that a sign-in happened, and of exactly whom.
///
/// Obtained only from [`Session::ticket`], which is only ever `Some` for a
/// session that has already presented a password. There is no other
/// constructor, so a ticket cannot be assembled by a caller who has not signed
/// in — including one inside this workspace.
///
/// Redeemed by [`Session::resume`], which re-reads the user and refuses when
/// the record has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// The text the holder presents to take this up again.
    bearer: String,
    /// The user as they were when the password was checked.
    ///
    /// Held whole rather than as an id, because the id alone would say *who*
    /// and the point of the comparison is *whether they are still the same*.
    held: Box<UserDefinition>,
}

impl Ticket {
    /// The text to hand the holder, and to present again later.
    ///
    /// Unguessable and meaningless: it says nothing about the user it stands
    /// for, so a token in a log is a token and not a name.
    #[must_use]
    pub fn bearer(&self) -> &str {
        &self.bearer
    }
}

impl Session<'_> {
    /// A ticket for the identity this session has already proved.
    ///
    /// `None` for a session that has not signed in. That is not an oversight to
    /// paper over with an empty ticket: on an open store an anonymous session
    /// may do anything, and minting a durable token out of the *absence* of a
    /// credential would turn "this store has no users yet" into "here is a key
    /// that still works after it does".
    #[must_use]
    pub fn ticket(&self) -> Option<Ticket> {
        let Identity::Signed(who) = &self.identity else {
            return None;
        };
        Some(Ticket {
            bearer: token(),
            held: who.clone(),
        })
    }

    /// Take up the identity a ticket stands for.
    ///
    /// Reads the user back and requires the record to be exactly what it was
    /// when the ticket was cut. No password is checked, because one already was
    /// — that is the point — and nothing is asked of the throttle, because
    /// nothing is being guessed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TicketStale`] when the user has been changed or
    /// removed, and a substrate failure when the catalog cannot be read.
    pub fn resume(&mut self, ticket: &Ticket) -> Result<()> {
        let mut transaction = self.store.begin()?;
        let found = Catalog::new(&mut transaction)
            .users()?
            .into_iter()
            .find(|user| user.id == ticket.held.id);
        transaction.rollback();

        // Equality across the whole record, not field by field: a field added
        // to `UserDefinition` later is then covered by default rather than
        // covered once somebody remembers this line exists.
        let Some(user) = found.filter(|user| user == ticket.held.as_ref()) else {
            log::info!("a token for {} is no longer current", ticket.held.name);
            return Err(Error::TicketStale);
        };
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }
}

/// A fresh token, as hexadecimal.
fn token() -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let mut text = String::with_capacity(TOKEN_BYTES.saturating_mul(2));
    for byte in bytes {
        // `write!` on a `String` cannot fail, and the two-character form is
        // what keeps the length fixed — a token whose length varied with its
        // value would leak a bit of it.
        text.push(nibble(byte >> 4));
        text.push(nibble(byte & 0x0f));
    }
    text
}

/// One hexadecimal digit.
///
/// The range on each arm is what makes the addition sound — the caller only
/// ever passes a nibble — and the saturating form says so to the compiler as
/// well as to a reader, the same way `basic::sextet` does on the way back.
const fn nibble(value: u8) -> char {
    match value {
        0..=9 => b'0'.saturating_add(value) as char,
        _ => b'a'.saturating_add(value.saturating_sub(10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::{TOKEN_BYTES, nibble, token};

    #[test]
    fn a_token_is_fixed_length_hexadecimal() {
        let text = token();
        assert_eq!(text.len(), TOKEN_BYTES * 2);
        assert!(text.chars().all(|character| character.is_ascii_hexdigit()));
        assert!(
            text.chars()
                .all(|character| !character.is_ascii_uppercase())
        );
    }

    #[test]
    fn two_tokens_are_not_the_same_token() {
        // Not a test of the operating system's randomness — a test that this
        // function asks for it each time rather than caching one.
        assert_ne!(token(), token());
    }

    #[test]
    fn every_nibble_has_a_digit() {
        assert_eq!(nibble(0), '0');
        assert_eq!(nibble(9), '9');
        assert_eq!(nibble(10), 'a');
        assert_eq!(nibble(15), 'f');
    }
}
