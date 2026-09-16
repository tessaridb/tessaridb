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
use tessari_types::Epoch;

/// A copy this node does not hold, and where to find it.
///
/// All three halves travel, and the node id is the one that makes the redirect
/// checkable on arrival: a client sent to an address alone has no way to notice
/// that it met a different node than the one it was promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The address to dial — the same string the declaration carried.
    pub endpoint: String,
    /// Who was last heard there.
    pub node: [u8; NODE_ID_LEN],
    /// The leadership that node itself last claimed was current.
    ///
    /// Not this node's own leadership, and the distinction is the whole reason
    /// the field is worth carrying. A node deciding that somebody else's copy is
    /// fresher is, by construction, a node whose own copy failed the bound — it
    /// may hold no lease at all, so its own epoch is either absent or irrelevant.
    /// What it does hold is the last thing the named peer said about itself, and
    /// that is the value a redirect can be **checked** against: the client
    /// presents an epoch the target published, so a target that has since moved
    /// on can refuse it and name the newer one instead of failing opaquely.
    pub epoch: Epoch,
}

/// What this node knows about the copies it does not hold.
///
/// `Send + Sync` because one of these is shared by every connection a node
/// serves, and `Debug` because a [`crate::Session`] holding one is printed in
/// test failures.
pub trait Elsewhere: core::fmt::Debug + Send + Sync {
    /// A peer that says it may write, if this node knows of one.
    ///
    /// The second question, and it takes no argument because there is nothing
    /// to bound: a read that named the leader named a *node*, not a tolerance.
    ///
    /// **It is a required method and not a defaulted one.** A default answering
    /// `None` would let an implementation forget it and go on compiling, and the
    /// symptom would be a read that asked for the leader being refused on a
    /// cluster that has one — a wrong answer with nothing in an error state,
    /// which is the whole class this trait's one-method-per-question shape
    /// exists to avoid.
    fn writable(&self) -> Option<Peer>;

    /// A copy within `bound`, if this node knows of one.
    ///
    /// `None` means *not that I know of*, which a bounded read treats exactly as
    /// *nowhere*: a copy this node has never heard of is a copy of no known age,
    /// and a copy of no known age is outside every bound rather than inside the
    /// ones nobody measured.
    fn within(&self, bound: Duration) -> Option<Peer>;
}
