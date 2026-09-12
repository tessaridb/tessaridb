//! Where a read this node cannot answer could be answered instead.
//!
//! # Why this is a trait here rather than a directory here
//!
//! The thing that knows about peers is `tessari_wire::Directory`, and
//! `tessari-wire` depends on this crate. It cannot be the other way round: a
//! session is what a peer link *opens*, so the link is necessarily above it.
//!
//! So the session declares the one question it has — *do you know of a copy
//! within this bound?* — and the crate that greets peers answers it. One
//! method, because that is the whole of what a bounded read needs to know, and
//! a wider trait would be this crate guessing at a routing design it cannot see.
//!
//! # What the answer is NOT
//!
//! It is not *the freshest peer*, and it is not *a peer*. It is a peer **within
//! the bound the caller named**, which is §C-05's *exclude, never mark*: a node
//! beyond the bound is not a candidate that gets flagged, it is not a candidate.
//! An implementation that answered with its best guess would turn a promise back
//! into a hope, and the caller has no way to tell the two apart.
//!
//! # Why the bound is the only argument
//!
//! The session asks only **after** its own copy has failed the bound — that is
//! what makes the question worth asking. Handing the implementation this node's
//! own currency as well would invite it to re-decide *here*, which the session
//! has already decided, and two deciders of one question can disagree.
//!
//! The clock is the same story from the other end. Every method on the directory
//! takes `now` as a parameter so its ageing rule can be tested without waiting;
//! somebody has to read the real clock eventually, and that somebody is the
//! implementation at the edge, not this crate.

use core::time::Duration;

use tessari_encoding::NODE_ID_LEN;

/// A copy this node does not hold, and where to find it.
///
/// Both halves travel, and the node id is the half that makes the redirect
/// checkable: a client sent to an address alone has no way to notice that it
/// met a different node than the one it was promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The address to dial — the same string the declaration carried.
    pub endpoint: String,
    /// Who was last heard there.
    pub node: [u8; NODE_ID_LEN],
}

/// What this node knows about the copies it does not hold.
///
/// `Send + Sync` because one of these is shared by every connection a node
/// serves, and `Debug` because a [`crate::Session`] holding one is printed in
/// test failures.
pub trait Elsewhere: core::fmt::Debug + Send + Sync {
    /// A copy within `bound`, if this node knows of one.
    ///
    /// `None` means *not that I know of*, which a bounded read treats exactly as
    /// *nowhere*: a copy this node has never heard of is a copy of no known age,
    /// and a copy of no known age is outside every bound rather than inside the
    /// ones nobody measured.
    fn within(&self, bound: Duration) -> Option<Peer>;
}
