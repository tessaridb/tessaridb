//! The database handle every surface and process holds.

use super::*;

/// An open database.
///
/// Owns its store; a [`Session`] borrows from it. One process holds one `Db` and
/// opens as many sessions as it likes — which is the shape the layers below
/// already have, so the facade does not invent an owned session that would have
/// to reach through a lock to find the store again.
#[derive(Debug)]
pub struct Db {
    pub(super) store: Store,
    /// Who fetches the shards of a split table this node lacks (G033), set once
    /// by the process that knows its peers and handed to every session opened
    /// here — so every surface that serves a read, whichever it is, gathers.
    pub(super) gather: std::sync::OnceLock<Arc<dyn tessari_session::Gather>>,
    /// Who carries a record of a transaction across leaders (ADR-0112).
    pub(super) participants: std::sync::OnceLock<Arc<dyn tessari_session::Participants>>,
    /// Who carries a request this node cannot answer to the node that can
    /// (ADR-0108 D1), set once by the process that knows its peers.
    pub(super) coordinate: std::sync::OnceLock<Arc<dyn Coordinate>>,
    /// Who this node has heard leads, and where its peers are, for every
    /// session opened here — so a session on any surface can name the node that
    /// answers what it cannot (Q-863), not only a wire session.
    pub(super) elsewhere: std::sync::OnceLock<Arc<dyn tessari_session::Elsewhere>>,
    /// The cluster's one sign-in budget (ADR-0108 D5), for every session.
    pub(super) budget: std::sync::OnceLock<Arc<dyn tessari_session::Budget>>,
    /// The certificates this process presents, for `INFO FOR NODE` (ADR-0108 D9).
    pub(super) certificates: std::sync::OnceLock<Arc<dyn tessari_session::Certificates>>,
    /// What this store has landed, for whatever follows it — made on first
    /// asking, so a database nobody follows pays nothing on its commits.
    pub(super) commits: std::sync::OnceLock<Arc<feed::Commits>>,
    /// The folder `BACKUP … TO` writes into, set once by the process that was
    /// given one; unset, every `TO` is refused rather than written anywhere.
    pub(super) backups: std::sync::OnceLock<Arc<Path>>,
    /// The key the store is encrypted under, which also seals every backup
    /// this database produces (ADR-0108 D7). Fixed at open, like the store.
    pub(super) at_rest: Option<Arc<AtRestKey>>,
    /// When this node's settling pass first found each transaction's intents
    /// standing, in milliseconds since the Unix epoch (ADR-0112 D13a). Behind a
    /// lock rather than a concurrent map: only the settling pass touches it,
    /// one pass at a time, and never across a wait.
    pub(super) standing_since:
        std::sync::Mutex<std::collections::BTreeMap<tessari_storage::TransactionId, u64>>,
}
