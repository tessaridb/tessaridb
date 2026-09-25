//! The asking half of a gather: which node leads a shard, and how its records
//! are fetched a page at a time (G033, ADR-0083).
//!
//! # From the leader, found as a write is routed
//!
//! A shard is written by the leader of its governing line — the most specific
//! placed range containing it, else the store line (ADR-0082) — so that node
//! holds it by construction and is level with itself. The line comes from the
//! catalog's placements, and the node from the last greeting round, exactly as
//! collection finds a placed range's leader: [`crate::leader_of_range`] for a
//! placed line and [`crate::Directory::writable`] for the store's. No network
//! call decides *who*; one call per page asks.
//!
//! # All or nothing
//!
//! Any failure — no leader known, a refused dial, a refusal frame, a page out
//! of turn — is [`Unanswered::Refused`] naming what happened, and the session
//! refuses the whole read. A page that says more follows and holds nothing is
//! refused too: following it would ask the same question forever.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use rustls::pki_types::CertificateDer;
use tessari_encoding::NODE_ID_LEN;
use tessari_session::{Asked, Gather as Gathers, Gathered, Unanswered};
use tessari_storage::{Catalog, Reach};

use crate::driver::{Published, leader_of_range};
use crate::gathering::Gather;
use crate::link::{Answered, Ask, Credential, call};
use crate::peer::Hello;

/// What this node would tell a peer about itself, read when it is asked.
pub type Greeting = Box<dyn Fn() -> Result<Hello, String> + Send + Sync>;

/// Fetches the shards of a split table this node lacks from their leaders.
pub struct Gathering {
    /// The node's own store, for the placements and the member rows.
    ///
    /// Weak because the store is what holds this gatherer, and a strong pointer
    /// back would keep both alive after the process let go of them.
    db: Weak<tessaridb::Db>,
    /// This node's id, so a shard it leads itself is never dialled.
    me: [u8; NODE_ID_LEN],
    /// The peer credential and the authority that issued the cluster's.
    credential: Credential,
    authority: CertificateDer<'static>,
    /// Who was heard at the last greeting round.
    routing: Arc<Published>,
    /// This node's greeting, which opens every conversation on the link.
    greeting: Greeting,
}

impl core::fmt::Debug for Gathering {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gathering")
            .field("me", &self.me)
            .finish_non_exhaustive()
    }
}

impl Gathering {
    /// A gatherer for the node that holds `db`, speaking as `me`.
    #[must_use]
    pub fn new(
        db: &Arc<tessaridb::Db>,
        me: [u8; NODE_ID_LEN],
        (credential, authority): (Credential, CertificateDer<'static>),
        routing: Arc<Published>,
        greeting: Greeting,
    ) -> Self {
        Self {
            db: Arc::downgrade(db),
            me,
            credential,
            authority,
            routing,
            greeting,
        }
    }

    /// The leader of `shard`'s governing line and where to dial it.
    fn leader_of(&self, shard: Reach) -> Result<([u8; NODE_ID_LEN], String), String> {
        let db = self.db.upgrade().ok_or("this node is stopping")?;
        let mut transaction = db.store().begin().map_err(|why| why.to_string())?;
        let declared = Catalog::new(&mut transaction)
            .replicas()
            .map_err(|why| why.to_string())?;
        transaction.rollback();
        let placed: BTreeSet<Reach> = declared.iter().filter_map(|peer| peer.leads).collect();
        let line = tessari_storage::governing(&placed, shard);
        let directory = self.routing.current();
        let found = if line == Reach::Store {
            directory
                .writable()
                .map(|(endpoint, node)| (node, endpoint))
        } else {
            leader_of_range(line, &declared, &directory)
        };
        found.ok_or_else(|| format!("no leader of {line:?} has been heard"))
    }
}

impl Gathers for Gathering {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        let shard = Reach::Shard(asked.namespace, asked.database, asked.table, asked.shard);
        let (node, endpoint) = self.leader_of(shard).map_err(Unanswered::Refused)?;
        if node == self.me {
            return Err(Unanswered::Refused(
                "this node leads that shard's line and does not hold the shard".to_owned(),
            ));
        }
        let address: SocketAddr = endpoint.parse().map_err(|_| {
            Unanswered::Refused(format!(
                "the leader's endpoint is not an address: {endpoint}"
            ))
        })?;
        let said = (self.greeting)().map_err(Unanswered::Refused)?;
        let mut page = Gather {
            namespace: asked.namespace,
            database: asked.database,
            table: asked.table,
            shard: asked.shard,
            from: asked.window.from.cloned(),
            to: asked
                .window
                .to
                .map(|(id, inclusive)| (id.clone(), inclusive)),
            after: None,
        };
        let mut records = Vec::new();
        loop {
            let (_, answered) = call(
                address,
                self.credential.duplicate(),
                &self.authority,
                node,
                &said,
                Ask::Gather(&page),
            )
            .map_err(|why| Unanswered::Refused(format!("{endpoint}: {why}")))?;
            let Answered::Gathered(answered) = answered else {
                return Err(Unanswered::Refused(format!(
                    "{endpoint} answered something other than a page"
                )));
            };
            let more = answered.more;
            let empty = answered.records.is_empty();
            records.extend(answered.records);
            if records.len() > asked.most {
                return Err(Unanswered::Ceiling);
            }
            if !more {
                return Ok(Gathered { records, node });
            }
            if empty {
                return Err(Unanswered::Refused(format!(
                    "{endpoint} said more records follow and sent none"
                )));
            }
            page.after = records.last().map(|(id, _)| id.clone());
        }
    }
}
