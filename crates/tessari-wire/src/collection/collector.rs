//! The collecting side: which logs to ask for, and applying what arrives.

use super::{Collect, Collected};
use crate::error::{Error, Result};
use crate::link::{Answered, Ask, Credential, call};
use crate::peer::Hello;
use rustls::pki_types::CertificateDer;
use std::net::SocketAddr;
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{Catalog, Currency, Horizon, Reach, Store};
use tessari_types::Sequence;

/// Everything a node holds in order to collect from a peer.
///
/// A struct rather than six more arguments, for the reason [`crate::Standing`]
/// is one: [`call`] already takes six of its own.
#[derive(Debug)]
pub struct Collector<'a> {
    /// What this node shows the peer, and the key proving it is ours.
    pub mine: &'a Credential,
    /// The authority the peer's credential must chain to.
    pub authority: &'a CertificateDer<'a>,
    /// The greeting that opens the connection.
    pub said: &'a Hello,
    /// The peer to collect from, by id and address.
    pub peer: ([u8; NODE_ID_LEN], SocketAddr),
    /// The most records one collection may carry.
    ///
    /// It is also what makes *level* observable: a peer serves
    /// `min(limit, available)`, so an answer shorter than this is the peer
    /// saying it had no more. See [`Currency`].
    pub limit: u64,
}

impl Collector<'_> {
    /// Collect once from the peer starting at `from`, apply what comes back,
    /// and answer how far this node now reaches.
    ///
    /// # The cursor is the caller's and not this module's
    ///
    /// `from` is the first position this node does not hold **in `home`** —
    /// ordinarily its own committed tail there plus one. A node holds one log
    /// per home, so the pair travels together: a number without the log it
    /// counts in names no position at all. It is a parameter rather than something read
    /// here, and the rule that made it one is worth keeping: no surface on the
    /// network may reach the store's raw feed, `committed_tail` included, because
    /// a serving surface able to read positions directly is one that can stream
    /// records past every grant in the store. A collector is not a serving
    /// surface, but that rule has no exemptions on purpose — and obeying it put
    /// the cursor where the rest of this module already said it belonged, beside
    /// the clock that decides *when* to collect.
    ///
    /// # The predecessor is derived down the batch, not carried per record
    ///
    /// The answer states the leadership before its FIRST record; every record
    /// after that is preceded by the one just applied, so its epoch is the
    /// claim. Deriving is what makes the batch self-checking — a chain compared
    /// position by position refuses a substituted middle record, where a
    /// predecessor carried alongside each record would only restate what the
    /// record already says about itself.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Uncollectable`] when the peer cannot state what precedes
    /// this node's position — which is the answer to *bootstrap me* and not to
    /// *catch me up* — whatever [`call`] refuses with, and [`Error::Refused`]
    /// carrying the store's own words when a record will not apply. A record
    /// that disagrees with the history this node holds arrives as the store's
    /// own divergence, unchanged: rewording it would give an operator two
    /// accounts of one event.
    /// # Which log, and why the ANSWER names it
    ///
    /// This node asks for a **range**; the leader answers from the log it
    /// allocates into for that range and says which log that was. The ask could
    /// not carry the writer: a peer is reached and verified by its credential's
    /// identity, and it writes under its store's — two values a deployment keeps
    /// equal and the protocol must not assume are one field.
    ///
    /// So the records are filed where they were read from, which is the sentence
    /// `Store::apply_record_in` already makes about the home: a fact about the
    /// collect, and not a second authority over the record.
    pub fn collect(&self, into: &Store, home: Reach, from: Sequence) -> Result<Sequence> {
        self.round(into, &[(home, from)])
            .pop()
            .unwrap_or_else(|| Ok(Sequence::new(from.get().saturating_sub(1))))
    }

    /// Collect every `(home, from)` once, apply the answers together in their
    /// writer's commit order, and answer how far each home now reaches, in the
    /// order asked.
    ///
    /// # Why the logs are applied together
    ///
    /// One writer files its commits across these logs, and applying them one
    /// after another put a later commit of a coarser log before an earlier one of
    /// a finer log — a record both touched ended at the OLDER value here, with
    /// nothing in an error state (Q-796). The store's writer-order apply takes
    /// the whole round and applies only what every answer proves complete; what
    /// it withholds is asked for again next round (ADR-0084).
    ///
    /// # Errors
    ///
    /// Each home answers its own dial's refusal. A record the store refuses fails
    /// every home the round fetched, since the round is applied as one; what was
    /// applied before it stays, and each cursor stays where it was, so the next
    /// round re-offers records this node may already hold.
    pub fn round(&self, into: &Store, asks: &[(Reach, Sequence)]) -> Vec<Result<Sequence>> {
        let fetched: Vec<Result<Collected>> = asks
            .iter()
            .map(|(home, from)| self.fetch(into, *home, *from))
            .collect();
        self.apply(into, asks, fetched)
    }

    /// Apply one answer per ask — fetched by a round or sent on a held stream
    /// (ADR-0106 D5) — in the writer's commit order, and answer how far each
    /// home now reaches, in the order asked.
    ///
    /// The one apply path both share, so a record a stream delivers is applied
    /// exactly as the round would have applied it.
    pub fn apply(
        &self,
        into: &Store,
        asks: &[(Reach, Sequence)],
        fetched: Vec<Result<Collected>>,
    ) -> Vec<Result<Sequence>> {
        let applied = {
            let pages: Vec<tessari_storage::Page<'_>> = fetched
                .iter()
                .filter_map(|answer| answer.as_ref().ok())
                .map(|collected| tessari_storage::Page {
                    log: collected.log,
                    previous: collected.previous,
                    records: &collected.records,
                    horizon: self.horizon_of(collected),
                })
                .collect();
            into.apply_in_writer_order(&pages)
        };
        let mut applied = match applied {
            Ok(applied) => applied.into_iter(),
            Err(why) => {
                let message = why.to_string();
                return fetched
                    .into_iter()
                    .map(|answer| {
                        answer.and_then(|_| {
                            Err(Error::Refused {
                                message: message.clone(),
                            })
                        })
                    })
                    .collect();
            }
        };
        fetched
            .into_iter()
            .zip(asks)
            .map(|(answer, (_, from))| {
                let collected = answer?;
                let held = Sequence::new(from.get().saturating_sub(1));
                let reached = applied.next().flatten();
                let whole = reached == collected.records.last().map(|(at, _)| *at);
                // What the leader served this node under, recorded so the node
                // knows what it holds (G031, ADR-0081). After the records
                // applied: a refusal above leaves the old answer standing, which
                // errs toward answering fewer reads rather than more.
                if let Some(over) = collected.over {
                    into.record_served(over).map_err(refused)?;
                }
                // Level only when the leader had no more AND this node applied
                // all of it. A short answer is the one moment a node can observe
                // that its copy was current; a record held back for the next
                // round means it is not, whatever the leader said.
                let currency = if whole
                    && matches!(
                        self.horizon_of(&collected),
                        Horizon::Level(_) | Horizon::Unstated
                    ) {
                    Currency::Level
                } else {
                    Currency::Behind
                };
                let reached = reached.unwrap_or(held);
                into.collected(reached, currency);
                Ok(reached)
            })
            .collect()
    }

    /// What one answer proves about its log.
    ///
    /// Short means the peer had no more — unless it stopped on its byte budget,
    /// which only it knows and says. A full answer is contact and not arrival.
    pub(crate) fn horizon_of(&self, collected: &Collected) -> Horizon {
        let carried = u64::try_from(collected.records.len()).unwrap_or(u64::MAX);
        if carried < self.limit && !collected.stopped_early {
            collected.order.map_or(Horizon::Unstated, Horizon::Level)
        } else {
            Horizon::Full
        }
    }

    /// Ask the peer for `home` from `from`, and hand back its answer unapplied.
    pub(crate) fn fetch(&self, into: &Store, home: Reach, from: Sequence) -> Result<Collected> {
        let held = Sequence::new(from.get().saturating_sub(1));
        // Before the dial, not after it: a node that must not apply this log has
        // no business opening a connection for it, and refusing after the answer
        // would leave the leader holding a lag row for a follower that will never
        // apply a record (ADR-0078).
        //
        // Scoped to the store's own log, because that is where namespace
        // definitions arrive and `logs_to_collect` asks for it FIRST — so a
        // joining node cannot reach a namespace's log without passing here. An
        // unscoped check would refuse every later namespace-home collect of a
        // node that had just legitimately taken the cluster's namespaces, which
        // is the opposite of what this defends.
        if matches!(home, Reach::Store) && held == Sequence::ZERO {
            refuse_to_reinterpret(into)?;
        }
        let (_, answered) = call(
            self.peer.1,
            self.mine.duplicate(),
            self.authority,
            self.peer.0,
            self.said,
            Ask::Records(Collect {
                home,
                from,
                limit: self.limit,
            }),
        )?;
        let Answered::Collected(collected) = answered else {
            return Err(Error::OutOfTurn {
                tag: crate::peer::PeerFrame::Collected.tag(),
            });
        };
        // The log this collect read is the log it is applied into. The two were
        // allowed to differ while the frame named none: a namespace subscriber
        // read the leader's namespace log and filed every record in its OWN
        // store log, so the sequences counted in a counter they never came from
        // and nothing was in an error state to say so.
        Ok(collected)
    }
}

/// Every log this node should ask a leader for, in the order it should ask.
///
/// # Why the follower derives this instead of being told
///
/// A subscriber is entitled to the CHAIN from the store's own log down to the
/// reach it was granted: the namespace and database definitions its records
/// depend on are written in the logs above it, and a follower that asked only
/// for its own would hold records belonging to a namespace that does not exist
/// on it (Q-620, Q-621).
///
/// The grant lives on the leader and the follower does not hold it, so the
/// obvious design is for the leader to send the set. That would put the grant on
/// the wire beside the leader's own copy, and two copies of one authority is the
/// shape that lets a peer be authorized for one namespace and served another —
/// the objection [`Serving`]'s own header records.
///
/// It is not needed. The follower asks for [`Reach::Store`] first and applies
/// what comes back; the leader's read elided every mutation outside the grant,
/// so the catalog the follower then reads names only namespaces it is
/// subscribed to. The set derived from it is inside the grant **by
/// construction**, and the leader keeps the authority to refuse anything else.
///
/// The order is the store's log first, then each namespace, then its databases —
/// `Store::homes`'s order and the same reason: a definition arrives before the
/// records that depend on it.
///
/// # Errors
///
/// Returns [`Error::Refused`] carrying the store's own words when the catalog
/// cannot be read.
pub fn logs_to_collect(store: &Store) -> Result<Vec<Reach>> {
    let mut transaction = store.begin().map_err(refused)?;
    let catalog = Catalog::new(&mut transaction);
    let mut logs = vec![Reach::Store];
    let namespaces = catalog.namespaces().map_err(refused)?;
    for namespace in namespaces {
        logs.push(Reach::Namespace(namespace.id));
        for database in catalog.databases_in(namespace.id).map_err(refused)? {
            logs.push(Reach::Database(namespace.id, database.id));
            // Each shard of a split table is a log of its own (G031, ADR-0080),
            // asked for after its database because the table's definition — and
            // with it the map naming the shards — arrives in the logs above.
            // Leaving them out would hold every follower level on everything
            // except the records of a split table, with nothing in an error
            // state: the shard logs would simply never be asked for.
            for table in catalog
                .tables_in(namespace.id, database.id)
                .map_err(refused)?
            {
                let Some(shards) = &table.shards else {
                    continue;
                };
                // Retired shards too: a split stops new writes to a shard, not
                // the records already in its log (ADR-0095 D3).
                for shard in shards.logs() {
                    logs.push(Reach::Shard(namespace.id, database.id, table.id, shard));
                }
            }
        }
    }
    transaction.rollback();
    // Narrowed by what this node was last served under (G031, ADR-0081): a
    // follower of one shard replays its table's definition, which names every
    // shard, and asking for the siblings would be refused every round — a
    // permanent warning is one an operator learns to skip. Only ever narrows:
    // the leader still decides what each log carries.
    if let Some(over) = store.served() {
        logs.retain(|home| over.contains(*home) || home.contains(over));
    }
    Ok(logs)
}

/// Refuse the first store-level collect of a node that holds a tenancy of its
/// own, naming what would be reinterpreted.
///
/// # What makes it the FIRST collect
///
/// The position asked for. The caller seeds the cursor from the first position
/// it does not hold in the peer's log, so `from == 1` means this node has applied
/// nothing of that log — which is the only durable evidence of *joining* the
/// engine holds. A node joins by configuration plus a `DEFINE REPLICA` and then
/// simply starts collecting (W384), so there is no other instant to hang this on;
/// reading the configuration instead would refuse a node that joined correctly
/// last week, because the configuration is present on every restart.
///
/// The cursor is **told** to this function rather than read here, which is the
/// rule this module already lives under: deciding when to collect belongs with
/// the node's lifecycle, and `collection.rs` may not reach the raw feed at all —
/// `tessari-cli` computes the seed with the one `committed_tail` call the
/// enforcement suite classifies. So the refusal reads the parameter, and it
/// fires exactly once per peer log.
///
/// # Why a namespace is the whole test
///
/// A namespace is the ROOT of the record address space: a record lives at
/// `(namespace, database, table, id)`, and every database, table, index, field
/// and grant hangs beneath one. A node holding no namespace of its own holds no
/// record of its own, so the other levels whose allocated id is an ADDRESS are
/// covered transitively rather than each needing its own test.
///
/// The batch's content is deliberately not decoded. The hazard is the overlap of
/// two counters, not this particular answer: a leader that happens to have sent
/// nothing yet will allocate namespace 1 later, and a check that waited for the
/// colliding record to arrive would pass the collect that precedes it.
pub(crate) fn refuse_to_reinterpret(into: &Store) -> Result<()> {
    let mut transaction = into.begin().map_err(refused)?;
    let held = Catalog::new(&mut transaction)
        .namespaces()
        .map_err(refused)?;
    transaction.rollback();
    if held.is_empty() {
        return Ok(());
    }
    Err(Error::WouldReinterpret {
        held: held
            .iter()
            .map(|namespace| format!("`{}`", namespace.name))
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// The store's own words, carried through rather than reworded.
pub(crate) fn refused(why: tessari_storage::Error) -> Error {
    Error::Refused {
        message: why.to_string(),
    }
}
