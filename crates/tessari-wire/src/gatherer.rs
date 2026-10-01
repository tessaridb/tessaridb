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
pub type Greeting = Box<dyn Fn() -> Result<Hello, GreetingUnavailable> + Send + Sync>;

/// Why this node could not say what it would tell a peer.
#[derive(Debug)]
pub enum GreetingUnavailable {
    /// The node is stopping, so there is no store left to read.
    Stopping,
    /// The store could not be read.
    Store(tessari_storage::Error),
}

impl core::fmt::Display for GreetingUnavailable {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Stopping => formatter.write_str("this node is stopping"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for GreetingUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stopping => None,
            Self::Store(error) => Some(error),
        }
    }
}

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
        db: Arc<tessaridb::Db>,
        me: [u8; NODE_ID_LEN],
        (credential, authority): (Credential, CertificateDer<'static>),
        routing: Arc<Published>,
        greeting: Greeting,
    ) -> Self {
        Self {
            db: Arc::downgrade(&db),
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
        let said = (self.greeting)().map_err(|why| Unanswered::Refused(why.to_string()))?;
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
            pushed: asked.pushed.cloned(),
            enough: asked.enough.and_then(|enough| u64::try_from(enough).ok()),
            reduce: asked.reduce.cloned(),
            ordered: asked.ordered.cloned(),
        };
        let mut records = Vec::new();
        let mut partials = Vec::new();
        loop {
            let (_, answered) = call(
                address,
                self.credential.duplicate(),
                &self.authority,
                node,
                &said,
                Ask::Gather(&page),
            )
            .map_err(|why| match why {
                // The leader's map differs from the one this ask was built
                // from: said as itself, so the asker reads its map again
                // rather than looking for a leader that did not answer.
                crate::Error::NotGathered(crate::gathering::Ungathered::MapMoved) => {
                    Unanswered::Moved
                }
                other => Unanswered::Refused(format!("{endpoint}: {other}")),
            })?;
            let Answered::Gathered(answered) = answered else {
                return Err(Unanswered::Refused(format!(
                    "{endpoint} answered something other than a page"
                )));
            };
            // Folded: groups rather than records, each page resuming where
            // the leader's read stopped. A page that declined, or a leader that
            // sent records when it was asked for folds, sends the asker back to
            // the records.
            if asked.reduce.is_some() {
                let Some(tessari_session::Reduced::Partials(folded)) = answered.reduced else {
                    return Ok(Gathered {
                        records: Vec::new(),
                        node,
                        reduced: Some(tessari_session::Reduced::Declined),
                    });
                };
                partials.extend(folded);
                if partials.len() > asked.most {
                    return Err(Unanswered::Ceiling);
                }
                if !answered.more {
                    return Ok(Gathered {
                        records: Vec::new(),
                        node,
                        reduced: Some(tessari_session::Reduced::Partials(partials)),
                    });
                }
                let Some(resume) = answered.resume else {
                    return Err(Unanswered::Refused(format!(
                        "{endpoint} said more groups follow and gave no place to resume"
                    )));
                };
                page.after = Some(resume);
                continue;
            }
            let more = answered.more;
            let empty = answered.records.is_empty();
            records.extend(answered.records);
            if records.len() > asked.most {
                return Err(Unanswered::Ceiling);
            }
            // Enough is enough even from a leader that sent past it.
            if let Some(enough) = asked.enough
                && records.len() >= enough
            {
                records.truncate(enough);
                return Ok(Gathered {
                    records,
                    node,
                    reduced: None,
                });
            }
            if !more {
                return Ok(Gathered {
                    records,
                    node,
                    reduced: None,
                });
            }
            // A narrowed page may keep none of what it read, and says where it
            // got to; an un-narrowed one that sent nothing never got anywhere.
            match answered.resume {
                Some(resume) => page.after = Some(resume),
                None if empty => {
                    return Err(Unanswered::Refused(format!(
                        "{endpoint} said more records follow and sent none"
                    )));
                }
                None => page.after = records.last().map(|(id, _)| id.clone()),
            }
        }
    }
}
