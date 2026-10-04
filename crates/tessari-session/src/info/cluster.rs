//! INFO for this node and its stream consumers.

use std::collections::BTreeMap;

use tessari_ql::{Name, Span};
use tessari_storage::{BUILD_VERSION, Catalog, Feed, Transaction};
use tessari_types::Value;

use crate::error::{Error, Result};
use crate::session::Session;

use super::{
    described_consumer, described_failover, described_follower, described_leaders,
    described_replica, guarantees, running_state,
};

impl Session<'_> {
    /// This node's own settings, and the peers it knows.
    ///
    /// Needs `Administer`, decided by `Needs::of` before this runs, for the
    /// reason `$node` needs it: the subject names no table, so a grant loop
    /// passes over it vacuously, and neither half has a smaller truthful form.
    ///
    /// # The two groups are the answer, not a formatting choice
    ///
    /// The flat fields come from the `META` keyspace and describe **this
    /// machine**. Everything under `cluster` comes from the catalog and
    /// describes the **topology**. That is ADR-0018's line, and ADR-0020 §3 puts
    /// it in the shape of the answer on purpose: a reader has to be able to tell
    /// which fields would follow a backup and which would not, and flattening
    /// the two would make that a thing you have to remember rather than a thing
    /// you can see. The bad day it is remembered wrongly on is the one where
    /// last night's backup goes onto a fresh machine and two processes claim one
    /// identity.
    ///
    /// `membership` is **not** answered, and its absence is the decision. The
    /// type behind it carries exactly one variant, so the field could only ever
    /// report `alone` — on a single node, and equally on a node whose writes
    /// are being fenced for belonging to a cluster. A constant that reads as a
    /// claim is worse than no field, and this one was read as a claim: it is
    /// the first thing the console printed on its cluster tab, which is how it
    /// came to say a node stood alone while the engine refused its writes for
    /// not doing so. `roles` and `cluster.peers` answer the question people
    /// were asking this one, and they answer it from the authority the node
    /// actually holds. The persisted `Membership` stays where it is: it is
    /// on-disk identity format and the natural home for a real cluster name,
    /// which is a door for whoever designs cluster identity rather than for a
    /// response shape.
    pub(super) fn info_node(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<BTreeMap<String, Value>> {
        let identity = self.store.node_identity()?;
        let (retention, retained_by) = self.store.effective_retention()?;
        let catalog = Catalog::new(transaction);
        let peers = catalog
            .replicas()?
            .iter()
            .map(|replica| described_replica(replica, &catalog))
            .collect::<Result<Vec<_>>>()?;
        let peers = Value::Array(peers);
        // Asked of the catalog rather than computed from `peers` above, so that
        // what is reported and what a reopen would adopt are one answer to one
        // question. `null` when nothing names this node — which is a different
        // statement from an empty role set, and the difference is the whole
        // point: no row is *unbound*, an empty set is *drained*.
        let desired = match catalog.desired_roles(&identity.id)? {
            Some(roles) => Value::Array(roles.names().into_iter().map(Value::from).collect()),
            None => Value::Null,
        };
        // Asked of the store rather than the catalog: `REPLICAS` is what the
        // cluster was told, and this is what actually collected. A peer
        // declared and never seen appears in `peers` and not here, which is
        // the most useful thing either list says.
        // Asked through `health()` rather than of the lease directly, so that
        // this and `/metrics` are one answer to one question rather than two
        // that can drift.
        let held = self.store.health()?;
        let campaigns = held.campaigns;
        let across = across_report(&held);
        let lease = match held.lease_remaining {
            Some(left) => tessari_types::Duration::new(
                i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                left.subsec_nanos(),
            )
            .map_or(Value::Null, Value::Duration),
            None => Value::Null,
        };
        let leading = self.store.leading().map_or(Value::Null, |epoch| {
            Value::from(i64::try_from(epoch.get()).unwrap_or(i64::MAX))
        });
        // `null` when nobody has set a policy, and that is a different statement
        // from *the defaults*. A cluster nobody has configured runs the built-in
        // periods; reporting those here as a policy would make it impossible to
        // see whether one ever arrived — which is exactly the observation a
        // two-node check of replication is trying to make.
        // What every peer handshake refuses (ADR-0108 D6), as the catalog
        // holds it; a node applies the list within a few seconds of the row
        // reaching it.
        let revoked = Value::Array(
            Catalog::new(transaction)
                .revoked_certificates()?
                .into_iter()
                .map(|fingerprint| Value::from(fingerprint.as_str()))
                .collect(),
        );
        // Nodes removed from the cluster and never admitted again (ADR-0108 D9).
        let tombstoned = Value::Array(
            Catalog::new(transaction)
                .tombstoned_nodes()?
                .into_iter()
                .map(Value::Uuid)
                .collect(),
        );
        let failover = match Catalog::new(transaction).failover()? {
            None => Value::Null,
            Some(held) => described_failover(&held),
        };
        let upstream = self.store.upstream().map_or(Value::Null, |held| {
            Value::Object(BTreeMap::from([
                ("state".to_owned(), Value::from(held.state.name())),
                (
                    "copied_records".to_owned(),
                    Value::from(i64::try_from(held.copied_records).unwrap_or(i64::MAX)),
                ),
                (
                    "copies".to_owned(),
                    Value::from(i64::try_from(held.copies).unwrap_or(i64::MAX)),
                ),
            ]))
        });
        let leaders = described_leaders(&Catalog::new(transaction))?;
        let followers = self
            .store
            .follower_lag()?
            .into_iter()
            .map(described_follower)
            .collect();
        Ok(BTreeMap::from([
            (
                "id".to_owned(),
                Value::from(identity.record_id().to_string().as_str()),
            ),
            (
                // The **effective** role of §6.1, which is the adopted set as
                // the lease leaves it — asked of the store rather than read off
                // the identity, so that a node cannot report `writable` while
                // its fence refuses every write. `cluster.desired` below is
                // untouched by the lease and must be: the pair is only worth
                // anything while the two can differ.
                "roles".to_owned(),
                Value::Array(
                    self.store
                        .effective_roles()?
                        .names()
                        .into_iter()
                        .map(Value::from)
                        .collect(),
                ),
            ),
            (
                "version".to_owned(),
                Value::from(identity.version.to_string().as_str()),
            ),
            // The exact build beside the ordered version, for the same reason
            // it sits beside it in `$node`: an operator holding a pre-release
            // has to be able to see that they are holding one.
            ("build".to_owned(), Value::from(BUILD_VERSION)),
            (
                "endpoints".to_owned(),
                Value::Array(
                    identity
                        .endpoints
                        .iter()
                        .map(|endpoint| Value::from(endpoint.as_str()))
                        .collect(),
                ),
            ),
            (
                // Beside `endpoints` rather than under `cluster`, because it is
                // on the local side of ADR-0018's line: a disk budget describes
                // this machine and does not travel. `null` is *unbounded* — a
                // choice now, since the default is a window (ADR-0094 D2) —
                // and `retain_source` beside it says who made it.
                "retain".to_owned(),
                match retention {
                    tessari_storage::Retention::Keep(keep) => {
                        // Saturating rather than an `as` cast: the report is a
                        // number a person reads, and a width that wrapped would
                        // print a negative retention rather than fail.
                        Value::from(i64::try_from(keep.get()).unwrap_or(i64::MAX))
                    }
                    tessari_storage::Retention::Unbounded => Value::Null,
                },
            ),
            ("retain_source".to_owned(), Value::from(retained_by.name())),
            // Local, beside `endpoints`: what this machine presents, read from
            // the process now, so a renewal shows on the next report. The
            // fingerprint is the value `FINGERPRINT` pins and `REVOKE
            // CERTIFICATE` names, read off the node the way `id` is (D9).
            ("certificates".to_owned(), self.described_certificates()),
            // Beside it, how the clients are served, so a node in the clear says
            // so here and not only in a start line that scrolled away (ADR-0111).
            ("clients".to_owned(), self.described_clients()),
            (
                "cluster".to_owned(),
                Value::Object(BTreeMap::from([
                    ("peers".to_owned(), peers),
                    ("revoked".to_owned(), revoked),
                    ("tombstoned".to_owned(), tombstoned),
                    // On the replicated side of ADR-0018's line, because that is
                    // where it comes from: `roles` above is what this machine
                    // holds and a backup would not carry, `desired` is what the
                    // cluster says and every node does carry.
                    ("desired".to_owned(), desired),
                    // Beside the peers rather than inside them: a row here is
                    // about a follower that has collected, and `peers` is about
                    // what was declared. Joining them would put a lag figure on
                    // a peer that has never asked for anything.
                    ("followers".to_owned(), Value::Array(followers)),
                    // Each range's leader as the log recorded it, so an
                    // operator reads who leads what without asking every node.
                    ("leaders".to_owned(), leaders),
                    // The follower's own side (ADR-0094 D4): where this node
                    // stands against the peer it collects from. `null` on a
                    // node that has never collected nor copied, which is a
                    // node that follows nobody rather than one in sync.
                    ("upstream".to_owned(), upstream),
                    // `null` on a node nobody made a leader, which is a
                    // different statement from zero: a store standing alone is
                    // not a leader whose time has run out. When it is a
                    // duration it is the one the concept names as the
                    // split-brain signal — this at zero while writes are still
                    // being taken is the state the fence exists to prevent.
                    ("lease".to_owned(), lease),
                    // The other half of the pair an operator watches. A lease
                    // heading toward zero says *how long*, and this says *what
                    // for* — without it a report cannot distinguish a node
                    // renewing the leadership it already held from one that has
                    // just taken it from somebody else, which is the difference
                    // between a quiet cluster and a failover nobody saw.
                    //
                    // `null` on a node no round ever granted anything to, on the
                    // same reasoning as the lease beside it: not leading is a
                    // different statement from leading under the first epoch.
                    ("epoch".to_owned(), leading),
                    // The third of the pair, and the one that says whether the
                    // cluster is QUIET. A healthy cluster's followers do not
                    // stand against a leader they can hear (ADR-0066), so this
                    // staying flat while somebody holds a lease is the
                    // observable form of that rule — and a number climbing on a
                    // node that is not leading says the gate has stopped
                    // working, which nothing else here would show.
                    //
                    // Rounds STOOD and not rounds won: a round that loses is
                    // exactly the noise worth seeing. From the same `health()`
                    // the lease above comes from and `/metrics` reports, so the
                    // two surfaces cannot drift.
                    (
                        "campaigns".to_owned(),
                        Value::from(i64::try_from(campaigns).unwrap_or(i64::MAX)),
                    ),
                    // Transactions across leaders (ADR-0112 D11): how the ones
                    // this node coordinated ended, and what the last settling
                    // pass left standing here — `null` before the first pass,
                    // which is not a count of zero.
                    ("across".to_owned(), across),
                    // The periods this cluster waits before it replaces a
                    // leader, with the pair that orders two of them. Beside the
                    // lease and the epoch because it is what those two are
                    // measured against: a lease counting down says how long,
                    // and this says how long it was ever meant to be.
                    ("failover".to_owned(), failover),
                    // Each balanced table's shards as this node's balancing
                    // pass last counted them (ADR-0113 D4) — empty on every
                    // node but the store line's leader, which is the one that
                    // counts.
                    (
                        "balanced".to_owned(),
                        Value::Array(
                            self.store
                                .sampled_shards()
                                .iter()
                                .map(|(_, sampled)| {
                                    let mut described = super::described_sample(sampled);
                                    if let Value::Object(fields) = &mut described {
                                        fields.insert(
                                            "table".to_owned(),
                                            Value::from(sampled.name.as_str()),
                                        );
                                    }
                                    described
                                })
                                .collect(),
                        ),
                    ),
                ])),
            ),
        ]))
    }

    /// One consumer: what was declared, what this process is doing with it, and
    /// what it does not promise.
    ///
    /// Three named groups rather than one flat object, for `INFO FOR NODE`'s
    /// reason plus one of its own:
    ///
    /// - `declared` is what a backup carries and what every node agrees on;
    /// - `running` is this process only, and is empty on a node that has not
    ///   started it;
    /// - `guarantees` is here because the loudest failure of systems that ship
    ///   this feature is not a bug, it is that their delivery semantics are
    ///   documented somewhere other than where a person configures the thing.
    ///   Somebody reading this output is configuring it right now.
    pub(super) fn info_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
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
        let Feed::Kafka {
            brokers,
            topic,
            format,
        } = &consumer.feed
        else {
            return Err(Error::WrongConsumerKind {
                name: name.text.clone(),
                kind: "topic",
                instead: format!("INFO FOR TOPIC CONSUMER {}", name.text),
                span,
            });
        };
        let destination = self.named_table(transaction, &consumer)?;
        Ok(BTreeMap::from([
            (
                "declared".to_owned(),
                described_consumer(&consumer, (brokers, topic, format), &destination),
            ),
            (
                "running".to_owned(),
                running_state(self.store.running().progress(&consumer.name).as_ref()),
            ),
            // What `ON FAILURE quarantine` parked, kept in the store and so
            // here after any restart (Q-708).
            (
                "quarantine".to_owned(),
                self.parked(transaction, &consumer, &destination, span)?,
            ),
            ("guarantees".to_owned(), guarantees()),
        ]))
    }

    /// Every declared consumer, with whether this process is running it.
    ///
    /// The counters are left to `INFO FOR KAFKA CONSUMER <name>`: this is the listing
    /// an operator reads to find out *which* consumer to ask about, and a table
    /// of every partition position would bury that.
    pub(super) fn info_consumers(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<BTreeMap<String, Value>> {
        let declared = Catalog::new(transaction).consumers()?;
        let mut described = Vec::with_capacity(declared.len());
        for consumer in declared {
            // Kafka consumers only: a topic consumer is listed by the topic it
            // reads, in `INFO FOR TOPIC` (ADR-0087).
            let Feed::Kafka { topic, .. } = &consumer.feed else {
                continue;
            };
            let running = self.store.running().progress(&consumer.name).is_some();
            described.push(Value::Object(BTreeMap::from([
                ("name".to_owned(), Value::from(consumer.name.as_str())),
                ("topic".to_owned(), Value::from(topic.as_str())),
                ("group".to_owned(), Value::from(consumer.group.as_str())),
                ("running".to_owned(), Value::Bool(running)),
            ])));
        }
        Ok(BTreeMap::from([(
            "consumers".to_owned(),
            Value::Array(described),
        )]))
    }
}

impl Session<'_> {
    /// How this node serves its clients; `null` from a process serving none.
    fn described_clients(&self) -> Value {
        self.certificates
            .as_ref()
            .map_or(Value::Null, |certificates| {
                let transport = certificates.clients();
                Value::Object(BTreeMap::from([
                    ("tls".to_owned(), Value::Bool(transport.tls)),
                    ("required".to_owned(), Value::Bool(transport.required)),
                ]))
            })
    }

    /// What this node presents, one object per surface; empty when nothing.
    fn described_certificates(&self) -> Value {
        let presented = self
            .certificates
            .as_ref()
            .map(|certificates| certificates.presented())
            .unwrap_or_default();
        Value::Array(
            presented
                .into_iter()
                .map(|shown| {
                    Value::Object(BTreeMap::from([
                        ("surface".to_owned(), Value::from(shown.surface)),
                        (
                            "fingerprint".to_owned(),
                            Value::from(shown.fingerprint.as_str()),
                        ),
                        (
                            "expires".to_owned(),
                            shown.expires.map_or(Value::Null, |seconds| {
                                Value::Datetime(tessari_types::Datetime::from_seconds(seconds))
                            }),
                        ),
                    ]))
                })
                .collect(),
        )
    }
}

/// The `across` group of `INFO FOR NODE`, from the same `health()` the
/// `/metrics` scrape reads.
fn across_report(held: &tessari_storage::Health) -> Value {
    let count = |held: u64| Value::from(i64::try_from(held).unwrap_or(i64::MAX));
    let sampled = |held: Option<u64>| held.map_or(Value::Null, count);
    Value::Object(BTreeMap::from([
        ("committed".to_owned(), count(held.across_committed)),
        ("aborted".to_owned(), count(held.across_aborted)),
        ("in_doubt".to_owned(), count(held.across_in_doubt)),
        ("pending".to_owned(), sampled(held.across_pending)),
        ("with_intents".to_owned(), sampled(held.across_with_intents)),
    ]))
}
