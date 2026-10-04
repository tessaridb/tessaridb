//! Declaring nodes, failover, replicas and stream consumers.

use tessari_encoding::Roles;
use tessari_ql::{Name, Span};
use tessari_storage::{
    Catalog, ConsumerDefinition, Feed, Mapped, OnFailure, ReplicaDefinition, Transaction,
};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

use super::{Declared, FORMAT_JSON, Peer, named_roles};

impl Session<'_> {
    /// `DEFINE NODE ROLES … ENDPOINTS …` — the local half of the configuration.
    ///
    /// # It does not run in the transaction, and that is not an oversight
    ///
    /// The identity lives in the `META` keyspace, which is not the log
    /// (ADR-0018 §1). A `META` write therefore cannot be part of a log
    /// transaction, and taking `transaction` here to look symmetrical with
    /// `DEFINE REPLICA` would be a durability claim the substrate does not
    /// support — a `CANCEL` afterwards would leave the roles changed while the
    /// caller believed otherwise. `BACKUP` is store-scoped for the same reason
    /// and is spelled the same way.
    ///
    /// An unknown role is refused here rather than in the grammar, for the
    /// reason a vector distance is: which roles exist is the store's question,
    /// and this is where the store knows what it knows.
    pub(super) fn define_node(
        &self,
        roles: Option<&[Name]>,
        endpoints: Option<&[String]>,
        retain: Option<Option<u64>>,
    ) -> Result<Outcome> {
        let named = roles.map(named_roles).transpose()?;
        self.store.configure_node(
            named,
            endpoints.map(<[String]>::to_vec),
            retain.map(|keep| keep.map(tessari_types::Sequence::new)),
        )?;
        Ok(Outcome::Done)
    }

    /// `DEFINE REPLICA second AT '…'` — the replicated half.
    ///
    /// This one **does** run in the transaction, because a peer is a catalog
    /// record: it commits with whatever else the script did and reaches every
    /// node through the ordinary apply path (ADR-0009).
    /// `DEFINE FAILOVER …` — the periods this cluster waits, as a log record.
    ///
    /// # The ordering pair is supplied here and never typed
    ///
    /// `epoch` is the leadership this node currently holds and `version` is one
    /// past whatever the stored row last carried. Neither is a clause, because
    /// an operator able to type either could write a policy that outranks a
    /// successor's — which is the exact case the epoch exists to settle, arriving
    /// through the statement meant to configure it.
    ///
    /// # Why the version is read inside this transaction
    ///
    /// The read and the write are one transaction, so two leaders setting a
    /// policy concurrently cannot both compute the same next version from the
    /// same stored row. They could still both be at the same epoch only if they
    /// were both leading it, which the lease already prevents.
    ///
    /// # The relations are checked by the policy, not here
    ///
    /// `Failover::stated` is the only way to build a policy that is not the
    /// default, and it is what knows which direction each relation fails in. A
    /// second check here would be a second answer that drifts.
    pub(super) fn define_failover(
        &self,
        transaction: &mut Transaction<'_>,
        periods: [tessari_types::Duration; 5],
        balance_leaderships: bool,
        span: Span,
    ) -> Result<Outcome> {
        let mut held = [std::time::Duration::ZERO; 5];
        for (slot, stated) in held.iter_mut().zip(periods) {
            // The parser refuses a period that is zero or negative, so the
            // seconds are known non-negative here and the conversion cannot be
            // lossy — the same reasoning the query budget states about its own
            // ceiling.
            let seconds = u64::try_from(stated.seconds()).unwrap_or(0);
            *slot = std::time::Duration::new(seconds, stated.nanos());
        }
        let [awareness, collection, round, campaign, lease] = held;
        let policy =
            tessari_storage::Failover::stated(awareness, collection, round, campaign, lease)
                .map_err(|why| Error::FailoverRefused {
                    reason: why.to_string(),
                    span,
                })?;
        let mut catalog = Catalog::new(transaction);
        // `None` means nobody has ever set one, and the first policy is version
        // zero rather than one: the number counts settings under a leadership,
        // and this is the first.
        let version = catalog
            .failover()?
            .map_or(0, |held| held.version.saturating_add(1));
        // A node holding no leadership writes under `Epoch::ZERO`, which is what
        // a store standing alone genuinely holds. It is not refused here: the
        // leadership gate already refuses a clustered node with no lease before
        // any write reaches this point, and refusing again would make a
        // single-node deployment unable to configure itself.
        let epoch = self.store.leading().unwrap_or(tessari_types::Epoch::ZERO);
        catalog.set_failover(policy, epoch, version, balance_leaderships)?;
        Ok(Outcome::Done)
    }

    /// `REVOKE CERTIFICATE '<sha256>'` — a row every node holds, from which
    /// its peer link refuses the certificate in both directions (ADR-0108 D6).
    ///
    /// In the transaction, for the failover policy's reason: it is a catalog
    /// record, commits with the rest of the script and reaches every node
    /// through the log. Revoking a certificate already revoked writes the same
    /// row again and is not refused — the list says the same thing afterwards.
    pub(super) fn revoke_certificate(
        transaction: &mut Transaction<'_>,
        fingerprint: &str,
    ) -> Outcome {
        Catalog::new(transaction).revoke_certificate(fingerprint);
        Outcome::Done
    }

    pub(super) fn define_replica(
        &self,
        transaction: &mut Transaction<'_>,
        peer: &Peer<'_>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .any(|found| found.name == peer.name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        // The words are read before the name is claimed, so a misspelled role
        // leaves nothing behind: the statement either declares the peer it was
        // asked for or declares nothing.
        let roles = peer
            .roles
            .map(named_roles)
            .transpose()?
            .unwrap_or(Roles::NONE);
        // A subscription on a row that names no node used to be refused here,
        // on the reasoning that a grant needs somebody to hold it and the peer
        // door — which looks a follower up by the id its certificate proved —
        // would never find one written against nobody. That was true while
        // nothing could ever bind such a row, and W282 is the wave that makes it
        // false: the row is bound by the first inbound greeting, and the grant
        // becomes findable at the moment the peer arrives (Q-611).
        //
        // It is inert until then rather than broad: `Subscriptions::granted`
        // matches `row.node == Some(follower)`, so an unbound row answers
        // nobody. What changes is only *when* the grant takes effect, never who
        // it can reach — and the recipient is still a node this cluster issued a
        // peer credential to, which is the act that admits a member.
        let replicates = match peer.replicates {
            None => None,
            Some(named) => Some(self.reach_of(transaction, named)?),
        };
        // Resolved by the same reader, so `LEADS SHARD` names a shard the table
        // has or is refused exactly as `REPLICATES SHARD` is (ADR-0082).
        let leads = match peer.leads {
            None => None,
            Some(named) => Some(self.reach_of(transaction, named)?),
        };
        Catalog::new(transaction).create_replica_with(ReplicaDefinition {
            name: peer.name.text.clone(),
            endpoint: peer.endpoint.to_owned(),
            roles,
            node: peer.node,
            replicates,
            leads,
            clients: peer.clients.map(str::to_owned),
            http: peer.http.map(str::to_owned),
            fingerprint: peer.fingerprint.map(str::to_owned),
            join: None,
            releasing: false,
            preferred: peer.preferred,
            region: peer.region.map(str::to_owned),
        })?;
        Ok(Outcome::Done)
    }

    /// `DROP REPLICA warsaw` — stops counting an endpoint as a peer.
    ///
    /// Nothing depends on a peer the way a field depends on an analyzer, so
    /// there is no refusal here: a replica declaration is a statement about who
    /// we send to, and withdrawing it is complete on its own.
    pub(super) fn drop_replica(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        if !Catalog::new(transaction).drop_replica(&name.text)? {
            return Err(Error::Unknown {
                entity: "replica",
                name: name.text.clone(),
                span,
            });
        }
        Ok(Outcome::Done)
    }

    /// `CREATE JOIN TOKEN FOR REPLICA r EXPIRES 10m` (ADR-0108 D9).
    ///
    /// The token is answered once and never stored: the row keeps its SHA-256
    /// and its expiry, so the catalog — in every backup and on every follower —
    /// holds nothing that binds a row. A second token for the row replaces the
    /// first, which is how a lost one is withdrawn.
    pub(super) fn create_join_token(
        transaction: &mut Transaction<'_>,
        replica: &Name,
        expires: tessari_types::Duration,
        span: Span,
    ) -> Result<Outcome> {
        let mut secret = [0_u8; 32];
        crate::generate::fill(&mut secret).map_err(|_| Error::TokenUnavailable {
            reason: "the operating system's randomness source could not be read",
            span,
        })?;
        let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        // The digest of the token's bytes — what a joiner offers on the peer
        // link — and not of the hex an operator copies.
        let digest: String = <sha2::Sha256 as sha2::Digest>::digest(secret)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            });
        // The parser refuses a length of zero or less, so the seconds are
        // non-negative; a life past the clock's range saturates rather than
        // wrapping into the past.
        let life_ms = expires
            .seconds()
            .saturating_mul(1_000)
            .saturating_add(i64::from(expires.nanos() / 1_000_000));
        let ticket = tessari_storage::JoinTicket {
            digest,
            expires_ms: now_ms.saturating_add(life_ms),
        };
        if !Catalog::new(transaction).wait_for_join(&replica.text, ticket)? {
            return Err(Error::Unknown {
                entity: "replica",
                name: replica.text.clone(),
                span,
            });
        }
        Ok(Outcome::Value(tessari_types::Value::String(token)))
    }

    /// `ALTER REPLICA b LEADS …` — moves a placement (ADR-0098).
    ///
    /// The range is resolved by the reader `DEFINE REPLICA` uses, so a shard the
    /// table lacks is refused the same way; what may be taken from whom is the
    /// catalog's rule.
    pub(super) fn alter_replica(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        change: &tessari_ql::ReplicaChange,
        span: Span,
    ) -> Result<Outcome> {
        use tessari_ql::ReplicaChange;
        let amended = match change {
            ReplicaChange::Leads { range, preferred } => {
                let leads = match range {
                    None => None,
                    Some(named) => Some(self.reach_of(transaction, named)?),
                };
                Catalog::new(transaction).alter_replica_leads(&name.text, leads, *preferred)?
            }
            // Read before the row is touched, so a misspelled role changes
            // nothing — `DEFINE REPLICA`'s order.
            ReplicaChange::Roles(words) => {
                let roles = named_roles(words)?;
                Catalog::new(transaction).amend_replica(&name.text, |row| row.roles = roles)?
            }
            ReplicaChange::At(endpoint) => Catalog::new(transaction)
                .amend_replica(&name.text, |row| row.endpoint.clone_from(endpoint))?,
            ReplicaChange::ClientsAt(clients) => Catalog::new(transaction)
                .amend_replica(&name.text, |row| row.clients.clone_from(clients))?,
            ReplicaChange::HttpAt(http) => Catalog::new(transaction)
                .amend_replica(&name.text, |row| row.http.clone_from(http))?,
            ReplicaChange::Region(region) => Catalog::new(transaction)
                .amend_replica(&name.text, |row| row.region.clone_from(region))?,
        };
        if !amended {
            return Err(Error::Unknown {
                entity: "replica",
                name: name.text.clone(),
                span,
            });
        }
        Ok(Outcome::Done)
    }

    /// Declare a consumer, after checking everything it names actually exists.
    ///
    /// The order matters and is the same one `DEFINE REPLICA` uses for its
    /// roles: everything that can be refused is refused **before** the name is
    /// claimed, so a statement either declares the consumer it was asked for or
    /// declares nothing. Here that covers three things a mistyped statement gets
    /// wrong — an unknown format, a destination that does not exist, and a
    /// mapping that names the same record field twice.
    pub(super) fn define_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &Declared<'_>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .consumers()?
                .iter()
                .any(|found| found.name == declared.name.text)
        {
            return Ok(Outcome::Done);
        }

        // Refused where the store knows what it knows, with the span the author
        // can see — the rule a vector distance and a node role already follow.
        // There is one format today, and an unknown one is a consumer that would
        // start and then fail on its first message rather than at declaration.
        if declared.format.text != FORMAT_JSON {
            return Err(Error::Unknown {
                entity: "message format",
                name: declared.format.text.clone(),
                span: declared.format.span,
            });
        }

        // The destination is resolved rather than remembered, which is what
        // removes the race the two-object design cannot: a consumer whose
        // destination does not exist is refused here instead of starting and
        // discovering it later, with messages already read.
        let (context, destination) = self.resolve_table(transaction, declared.destination)?;

        let mapping = mapped(declared.mapping)?;

        let definition = ConsumerDefinition {
            // Replaced by the catalog when the record is written; the field
            // exists on the way in only because the definition is one type.
            id: 0,
            name: declared.name.text.clone(),
            feed: Feed::Kafka {
                brokers: declared.source.brokers.clone(),
                topic: declared.source.topic.clone(),
                format: declared.format.text.clone(),
            },
            group: declared.group.to_owned(),
            identity: declared.identity.path.to_string(),
            mapping,
            namespace: context.namespace,
            database: context.database,
            destination,
            on_failure: failure_policy(declared.on_failure),
            // `None` reads as one, not as "decide for me". The parser has
            // already refused a zero, so this cannot be a consumer that runs
            // nothing.
            parallelism: declared.parallelism.unwrap_or(1),
            // Whose authority the writes will carry. `None` only on an open
            // store, where there is nobody to record and nothing to enforce —
            // the same condition under which the first user is declared.
            declarer: self.identity.user().map(|user| user.id),
        };
        // A name already taken is refused by the catalog itself, which is where
        // every other declaration's collision is decided.
        Catalog::new(transaction).create_consumer(&definition)?;
        Ok(Outcome::Done)
    }

    /// Forget a consumer.
    ///
    /// Removing the declaration is all this does here. Stopping whatever is
    /// running is the runner's job, and it learns of the change the same way a
    /// follower does — by reading the catalog — rather than by being called from
    /// inside a transaction that has not committed yet.
    pub(super) fn drop_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .consumers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(consumer) = found else {
            return Err(Error::Unknown {
                entity: "consumer",
                name: name.text.clone(),
                span,
            });
        };
        if matches!(consumer.feed, Feed::Topic { .. }) {
            return Err(Error::WrongConsumerKind {
                name: name.text.clone(),
                kind: "topic",
                instead: format!("DROP TOPIC CONSUMER {}", name.text),
                span,
            });
        }
        Catalog::new(transaction).drop_consumer(&consumer)?;
        Ok(Outcome::Done)
    }
}

/// A consumer's mapping, refused when two message fields land on one record
/// field — a result that would depend on which is applied last, and there is
/// no ordering that is not arbitrary.
pub(super) fn mapped(pairs: &[tessari_ql::FieldMapping]) -> Result<Vec<Mapped>> {
    let mut mapping = Vec::with_capacity(pairs.len());
    for pair in pairs {
        if mapping.iter().any(|held: &Mapped| held.to == pair.to.text) {
            return Err(Error::DuplicateMapping {
                field: pair.to.text.clone(),
                span: pair.to.span,
            });
        }
        mapping.push(Mapped {
            from: pair.from.path.to_string(),
            to: pair.to.text.clone(),
        });
    }
    Ok(mapping)
}

/// The stored failure policy for the one a statement wrote.
pub(super) const fn failure_policy(written: tessari_ql::OnFailure) -> OnFailure {
    match written {
        tessari_ql::OnFailure::Stop => OnFailure::Stop,
        tessari_ql::OnFailure::Quarantine => OnFailure::Quarantine,
    }
}
