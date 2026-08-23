//! The node's own identity: generated once, read at every open.
//!
//! This is the one thing in the store that a backup deliberately does not carry.
//! Everything else here is a function of the log, and a restore reproduces it by
//! replaying; the identity is in the `META` keyspace instead, precisely so that
//! restoring last night's backup onto a fresh machine produces a *different*
//! node rather than a second claimant to the first one's id (ADR-0018 §1).
//!
//! # Where the bytes come from, and why there is no fallback
//!
//! The operating system's randomness source, read directly. No crate, because
//! sixteen bytes from a file is not worth a dependency and this workspace's
//! dependency set is a measured budget rather than a habit.
//!
//! A source that cannot be read is a **refusal to open**. The tempting fallback
//! — a timestamp, a process id, a counter — is what turns "this cannot happen"
//! into a collision on the day two containers start in the same millisecond from
//! the same image, and a colliding node id fails silently: both processes serve,
//! both claim the id, and the routing built on it is wrong with nothing saying
//! so. A store that will not start says so once, here.

use std::fs::File;
use std::io::Read;
use std::sync::Arc;

use bgv_db_encoding::{
    NODE_ID_LEN, NodeIdentity, NodeIdentityKey, NodeVersion, Roles, StoreKey, StoreValue,
};
use bgv_db_kv::{KvBackend, WriteBatch};

use crate::error::{Error, Result};

/// Where the identifier's bytes come from.
const ENTROPY_SOURCE: &str = "/dev/urandom";

/// Read this node's identity, generating it once if the store has none.
///
/// Called at every open, not only at creation: a store written by a build that
/// predates node identity has a format version and no identity, and it gets one
/// the first time this build opens it rather than answering `$node` with nothing.
///
/// # Errors
///
/// Returns [`Error::NoEntropy`] when the randomness source cannot be read, the
/// substrate's failure when the write is refused, and a decoding failure when
/// the stored identity was written by a build this one cannot read.
pub(crate) fn ensure(backend: &Arc<dyn KvBackend>) -> Result<NodeIdentity> {
    if let Some(mut found) = read(backend)? {
        let running = NodeVersion::current();
        if found.version != running {
            // **This is the upgrade, and this is where it is visible.** The id
            // never moves; the version does, exactly once per binary change, and
            // the store has not served anything yet when we get here. A data
            // migration keyed on `found.version` belongs on this branch and
            // nowhere else — afterwards the evidence that an upgrade happened is
            // gone, because the only record of the old version is the one being
            // overwritten on the next line.
            found.version = running;
            write(backend, &found)?;
        }
        return Ok(found);
    }
    let fresh = NodeIdentity::alone(generate()?);
    // `Absent` for the same reason the format version uses it: two processes
    // opening the same new store both find nothing, and exactly one of them is
    // allowed to write. The loser's open fails, which is what already happens
    // to it one line earlier in `write_initial_metadata`.
    let batch = WriteBatch::new()
        .expect_absent(NodeIdentityKey::keyspace(), NodeIdentityKey.encode())
        .put(
            NodeIdentityKey::keyspace(),
            NodeIdentityKey.encode(),
            fresh.encode(),
        );
    backend.apply(batch)?;
    Ok(fresh)
}

/// Change what this node says it is for, and where it is reached.
///
/// The two settings a statement may move, and the two ADR-0018's replay test
/// puts on the local side: a replica that inherited `writable` from its source
/// would accept writes it must forward, and peers told to reach this machine at
/// the original's address would reach the original.
///
/// Absent clauses leave their field alone rather than clearing it, so
/// `DEFINE NODE ENDPOINTS …` is not a silent way to drop the roles. What each
/// clause **does** name replaces what was there — a list is the whole story, the
/// rule a grant's field list already follows — because there is no spelling for
/// "remove one role" that does not also need a spelling for "remove the last
/// one", and that question has no answer worth guessing at one node.
///
/// # Errors
///
/// Returns [`Error::NoIdentity`] when the store has none, and the substrate's
/// failure when the read or the write is refused.
pub(crate) fn configure(
    backend: &Arc<dyn KvBackend>,
    roles: Option<Roles>,
    endpoints: Option<Vec<String>>,
) -> Result<NodeIdentity> {
    let Some(mut identity) = read(backend)? else {
        // Unreachable through an open store, which resolves the identity before
        // it hands one out. Said rather than unwrapped, because "the open path
        // guarantees it" is a claim about another function that a later edit can
        // make false without touching this one.
        return Err(Error::NoIdentity);
    };
    if let Some(roles) = roles {
        identity.roles = roles;
    }
    if let Some(endpoints) = endpoints {
        identity.endpoints = endpoints;
    }
    write(backend, &identity)?;
    Ok(identity)
}

/// Replace the stored identity.
///
/// No `Absent` precondition here, because this one is an overwrite by
/// definition: the identity exists and one of its fields has moved.
fn write(backend: &Arc<dyn KvBackend>, identity: &NodeIdentity) -> Result<()> {
    let batch = WriteBatch::new().put(
        NodeIdentityKey::keyspace(),
        NodeIdentityKey.encode(),
        identity.encode(),
    );
    backend.apply(batch)?;
    Ok(())
}

/// The identity this store holds, if it holds one.
///
/// # Errors
///
/// Returns the substrate's failure, or a decoding failure when the stored bytes
/// carry a revision, role or membership this build does not know.
pub(crate) fn read(backend: &Arc<dyn KvBackend>) -> Result<Option<NodeIdentity>> {
    let key = NodeIdentityKey.encode();
    match backend.get(NodeIdentityKey::keyspace(), &key)? {
        Some(value) => Ok(Some(NodeIdentity::decode(value.as_slice())?)),
        None => Ok(None),
    }
}

/// Sixteen unpredictable bytes, or a refusal.
fn generate() -> Result<[u8; NODE_ID_LEN]> {
    let mut bytes = [0_u8; NODE_ID_LEN];
    let mut source = File::open(ENTROPY_SOURCE).map_err(|failure| Error::NoEntropy {
        path: ENTROPY_SOURCE,
        reason: failure.to_string(),
    })?;
    // `read_exact` rather than `read`: a short read would hand out an identifier
    // whose tail is zeroes, which is exactly the predictable id this refuses to
    // produce, and it would do it without an error.
    source
        .read_exact(&mut bytes)
        .map_err(|failure| Error::NoEntropy {
            path: ENTROPY_SOURCE,
            reason: failure.to_string(),
        })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use bgv_db_kv::MemoryBackend;

    use super::*;

    fn backend() -> Arc<dyn KvBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[test]
    fn a_store_with_no_identity_is_given_one() {
        let held = backend();
        assert!(read(&held).unwrap().is_none());
        let made = ensure(&held).unwrap();
        assert_eq!(read(&held).unwrap(), Some(made));
    }

    #[test]
    fn a_second_ensure_returns_the_first_identity_rather_than_a_new_one() {
        let held = backend();
        let first = ensure(&held).unwrap();
        let second = ensure(&held).unwrap();
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn a_node_that_starts_under_a_different_build_records_that_build() {
        // The upgrade path, and the thing that makes a migration possible at
        // all: the version moves while the id does not.
        //
        // The stored version is what **last ran**, not the highest ever seen, so
        // a downgrade rewrites it too. That is deliberate: the question a
        // migration asks on start is "what wrote the bytes I am about to read",
        // and the answer to that is the previous run whichever direction it went.
        //
        // The fixture is derived from the running version rather than written
        // down, because the package version is `0.0.0` and nothing is below it —
        // a hand-written "older" version would silently equal the current one and
        // the test would pass without the branch ever being taken.
        let held = backend();
        let first = ensure(&held).unwrap();
        let running = NodeVersion::current();
        let other = NodeVersion {
            major: running.major.saturating_add(1),
            ..running
        };
        assert_ne!(other, running, "the fixture must differ to test anything");
        write(
            &held,
            &NodeIdentity {
                version: other,
                ..first.clone()
            },
        )
        .unwrap();

        let after = ensure(&held).unwrap();
        assert_eq!(after.version, running);
        assert_eq!(after.id, first.id, "an upgrade is not a new identity");
    }

    #[test]
    fn configuring_a_node_moves_what_it_names_and_nothing_else() {
        let held = backend();
        let before = ensure(&held).unwrap();
        let after = configure(
            &held,
            Some(Roles::COORDINATING),
            Some(vec!["here:9000".to_owned()]),
        )
        .unwrap();

        assert_eq!(after.roles, Roles::COORDINATING);
        assert_eq!(after.endpoints, vec!["here:9000".to_owned()]);
        // The two fields a statement may not move. An identity that changed when
        // its roles did would be a session token again, and a version rewritten
        // by anything but a build change would make the upgrade branch in
        // `ensure` fire for reasons unrelated to an upgrade.
        assert_eq!(after.id, before.id);
        assert_eq!(after.version, before.version);
        assert_eq!(read(&held).unwrap(), Some(after));
    }

    #[test]
    fn an_absent_clause_leaves_its_field_where_it_was() {
        let held = backend();
        ensure(&held).unwrap();
        configure(&held, Some(Roles::COORDINATING), None).unwrap();
        let after = configure(&held, None, Some(vec!["here:9000".to_owned()])).unwrap();

        assert_eq!(
            after.roles,
            Roles::COORDINATING,
            "setting the endpoints cleared the roles"
        );
    }

    #[test]
    fn a_store_with_no_identity_cannot_be_configured() {
        // Unreachable through an open store. Asserted anyway, because the arm
        // that says so is a claim about a guarantee living in another function.
        assert!(matches!(
            configure(&backend(), Some(Roles::SERVING), None),
            Err(Error::NoIdentity)
        ));
    }

    #[test]
    fn two_generated_identifiers_differ() {
        // The check that stops every "it survived a restart" assertion above
        // from passing on a constant.
        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    #[test]
    fn a_generated_identifier_is_not_all_zeroes() {
        // A short read that went unnoticed would look exactly like this.
        assert_ne!(generate().unwrap(), [0_u8; NODE_ID_LEN]);
    }
}
