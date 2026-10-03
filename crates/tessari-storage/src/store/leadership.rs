//! Waiting for leadership, and refusing what another node leads.

use std::collections::BTreeSet;

use tessari_encoding::{NODE_ID_LEN, Roles};

use crate::catalog::Reach;
use crate::error::{Error, Result};

use super::Store;

impl Store {
    /// Whether this node is in a cluster and holds no leadership yet.
    ///
    /// The second half of *the effective role is the lease*, and it was missing
    /// until ADR-0064. The first half only ever **subtracted**: a node whose
    /// lease lapsed stopped being writable. But [`crate::lease::Held::spent`]
    /// answers `None` in two states that are not alike — *this lease is still
    /// open* and *this node was never given one* — so a node that had never won
    /// a round fell through to the adopted set and reported the `WRITABLE` the
    /// catalog declared.
    ///
    /// While exactly one node could stand that cost nothing: the only writable
    /// node was the only candidate. ADR-0063 widened the candidate set, and the
    /// only configuration in which a failover can produce a writer at all is one
    /// where every coordinating node is declared writable — at which point every
    /// one of them writes from the moment it opens, because none of them holds a
    /// lease and the two that lose a round never will. Three writers, and
    /// nothing anywhere in an error state.
    ///
    /// # Why the predicate is the catalog and not the role
    ///
    /// A store standing alone has no cluster to grant it anything, so a global
    /// *has no lease* rule would stop every existing single-node deployment
    /// accepting writes on the day it upgraded. That much is unchanged, and it
    /// is why there has to be a predicate at all.
    ///
    /// It used to be [`Roles::COORDINATING`], on the reasoning that
    /// [`Roles::ALONE`] is documented as *not `COORDINATING`, because there is
    /// nothing to coordinate with*, so the bit already drew the line between a
    /// member of a deciding set and a store on its own — one predicate, two
    /// rules that cannot drift apart. The line is drawn correctly and it
    /// answers the wrong question. Roles are a **set**: `SERVING | WRITABLE`
    /// without `COORDINATING` is a legal, ordinary declaration for a writable
    /// node that does not vote, and such a node standing in a cluster beside an
    /// elected leader was never fenced at all — the gate asked for a bit it does
    /// not carry, so it wrote freely and silently beside somebody else's
    /// leadership.
    ///
    /// *May this node take part in deciding* (ADR-0063's `stands`) and *could
    /// there be a leader other than me* are two questions, and only the first is
    /// about the role. The second is about the catalog:
    /// [`Self::in_a_cluster`], which is `names_a_peer` over the committed
    /// membership rows — already this engine's one spelling of it, and already
    /// the bound a joiner follows.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when the node
    /// identity cannot be read.
    pub fn awaiting_leadership(&self) -> Result<bool> {
        self.awaiting(&self.node_identity()?.id)
    }

    /// Whether this node holds no leadership and is not alone in holding none.
    ///
    /// Takes the identity rather than reading it, so the callers pay for one
    /// node read between them instead of one each — the commit gate reads the
    /// identity once and hands it to both questions it asks.
    ///
    /// # The lease is asked first because it is free
    ///
    /// [`crate::lease::Held::remaining`] is an in-memory read and the catalog is
    /// not, so a node that holds a leadership never reaches the second question
    /// — which is the state a leader is in for every commit it takes.
    pub(crate) fn awaiting(&self, me: &[u8; NODE_ID_LEN]) -> Result<bool> {
        if self.lease.remaining().is_some() {
            return Ok(false);
        }
        self.in_a_cluster(me)
    }

    /// Whether the committed catalog names a peer that is not this node.
    ///
    /// [`crate::names_a_peer`] over [`crate::Catalog::replicas`], and the
    /// definition carries the reasoning for both halves: why this is not *is the
    /// catalog empty*, and why the write gate asks this rather than asking what
    /// role the node was given.
    ///
    /// # Committed, deliberately, and it is the difference between joining and
    /// being unable to
    ///
    /// This opens its own transaction rather than reading through the one that
    /// is committing. A transaction sees its **own** pending writes, so the
    /// statement that declares the very first peer would find that peer while
    /// being judged, become clustered mid-commit, and refuse itself — leaving a
    /// standalone store with no way to join anything. A membership row that has
    /// not committed has not joined a cluster, so the committed state is also
    /// the answer that is true.
    ///
    /// Once it commits the node IS clustered and cannot write again until a
    /// round grants it something. That is the criterion, not a side effect.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when a stored
    /// membership row cannot be read.
    pub(crate) fn in_a_cluster(&self, me: &[u8; NODE_ID_LEN]) -> Result<bool> {
        let mut transaction = self.begin()?;
        let declared = crate::catalog::Catalog::new(&mut transaction).replicas()?;
        Ok(crate::catalog::another_node_may_write(&declared, me))
    }

    /// Refuse when the committed log names another node as the leader of a
    /// range this transaction writes.
    ///
    /// The half of the admission question the store-wide gate could not ask.
    /// [`Self::awaiting`] answers *does this node hold a leadership*, which was
    /// the whole question while a lease covered the whole store. It stopped
    /// being the whole question when two nodes could lead two namespaces: a node
    /// holding a perfectly live lease over one namespace was accepted writing
    /// into another node's, because nothing in the gate ever mentioned the range
    /// being written.
    ///
    /// # It is asked before the lease and not after it
    ///
    /// [`Self::awaiting`] returns early on a live lease, so a question asked
    /// after it never reaches a leader — and a leader writing into somebody
    /// else's namespace is exactly the case this exists to catch. Asking first
    /// costs a leader one scan per commit that it did not pay before, and that
    /// cost is the criterion rather than a side effect of it: a leader that
    /// writes without asking whose range this is, is the defect.
    ///
    /// # One scan, however many ranges the transaction touches
    ///
    /// The table is read once and every range resolved against the result by
    /// [`crate::catalog::covering`], rather than calling
    /// [`crate::Catalog::leader_of`] per range. The row count is the number of
    /// ranges a cluster has elected a leader for — O(members), not O(state) —
    /// so this is the same shape as the membership scan and not an engine
    /// introspection on a hot path.
    ///
    /// # The endpoint is read only when there is a refusal to build
    ///
    /// A [`crate::LeadershipDefinition`] carries the node and the epoch and no
    /// address; the address is on the membership row. Reading `system::REPLICAS`
    /// inside the refusal branch keeps the accepting path at one scan instead of
    /// two, which is the path every commit in a healthy cluster takes.
    ///
    /// # A strictly greater epoch supersedes the row
    ///
    /// The row says who led a range when it was written, and the epoch beside it
    /// says under which decision. A node that has since been granted a **higher**
    /// epoch is not writing into somebody else's range: it is writing into one
    /// whose recorded leader has been superseded, and ordering exactly that is
    /// what the epoch is for.
    ///
    /// Without this the gate is a latch rather than a gate. Recording a
    /// leadership is itself a write, so it meets this question — and the row that
    /// refuses it is the row it would replace. The first election in a cluster's
    /// life succeeds because no row exists yet; every one after it was refused
    /// permanently, while the epoch climbed without bound because the winner
    /// re-stood each time its unrecorded lease lapsed.
    ///
    /// **Equal and absent do not supersede.** A voter grants an epoch at most
    /// once and a round concludes only on a strict majority, so two nodes cannot
    /// hold the same epoch: a row naming another node at the epoch this node
    /// holds is a catalog disagreeing with itself, and the safe reading of that
    /// is the refusal. `None` is not zero either — a node nobody elected has
    /// nothing to supersede with, which is the state every redirect to a known
    /// leader is served from.
    ///
    /// # The epoch is the proved one, by construction
    ///
    /// [`Self::leading`] is written only by [`Self::hold`], which installs what a
    /// majority granted. The roundless [`Self::hold_lease`] sets a fence and no
    /// epoch at all. So nothing a caller asserts about itself in the frame being
    /// judged can reach this comparison.
    ///
    /// # One epoch per line, compared on the row's own line
    ///
    /// A placed range is a line of its own (ADR-0082), so a row's epoch is
    /// compared with this node's epoch on the line the row's range is governed
    /// by — the store line's for every unplaced range, which is every range of
    /// a store with no placement and therefore the comparison this always made.
    ///
    /// Answers the placed ranges it read, so the caller judges each written
    /// range's line against the same placement the rows were judged against.
    ///
    /// # Errors
    ///
    /// [`crate::Error::WriteIsElsewhere`] when another node leads one of the
    /// ranges, plus the substrate's failure and a decoding failure when a stored
    /// definition cannot be read.
    pub(crate) fn refuse_if_led_elsewhere(
        &self,
        ranges: &BTreeSet<Reach>,
        me: &[u8; NODE_ID_LEN],
    ) -> Result<BTreeSet<Reach>> {
        let mut transaction = self.begin()?;
        let catalog = crate::catalog::Catalog::new(&mut transaction);
        let held = catalog.leaderships()?;
        // The placement, read in the same transaction as the rows it carves:
        // the line a row belongs to and the line a write is judged on are one
        // question asked of one catalog (ADR-0082).
        let placed: BTreeSet<Reach> = catalog
            .replicas()?
            .into_iter()
            .filter_map(|peer| peer.leads)
            .collect();
        drop(transaction);
        // Every other node leading a range this writes, and whether this node
        // leads one too. One other leader and none of our own is a redirect;
        // anything more is a transaction no node may commit (G031 S2.4, Q-789).
        // The first range led elsewhere is not enough to answer: redirecting
        // there sends the client to a node that refuses the rest back.
        let mut elsewhere: Vec<crate::catalog::LeadershipDefinition> = Vec::new();
        let mut leads_one_here = false;
        for range in ranges {
            match self.led(&held, &placed, *range, me)? {
                Led::Unled | Led::Shared => {}
                Led::Here => leads_one_here = true,
                Led::Elsewhere(leader) => {
                    if !elsewhere.iter().any(|held| held.node == leader.node) {
                        elsewhere.push(leader);
                    }
                }
            }
        }
        if elsewhere.len() > 1 || (leads_one_here && !elsewhere.is_empty()) {
            let mut nodes: Vec<[u8; NODE_ID_LEN]> =
                elsewhere.iter().map(|leader| leader.node).collect();
            if leads_one_here {
                nodes.push(*me);
            }
            nodes.sort_unstable();
            return Err(Error::SpansLeaderships { nodes });
        }
        let Some(elsewhere) = elsewhere.pop() else {
            return Ok(placed);
        };
        let mut transaction = self.begin()?;
        let declared = crate::catalog::Catalog::new(&mut transaction).replicas()?;
        // A leadership the log carries whose node no membership row names is a
        // catalog that disagrees with itself. Refusing with an empty address is
        // still the right refusal — this node may not take the write — and it
        // says so rather than accepting it because the address was missing.
        let endpoint = declared
            .iter()
            .find(|peer| peer.node == Some(elsewhere.node))
            .map_or_else(String::new, |peer| peer.endpoint.clone());
        Err(Error::WriteIsElsewhere {
            endpoint,
            node: elsewhere.node,
            epoch: elsewhere.epoch,
        })
    }

    /// How current this node's copy is known to be, or `None` when that cannot
    /// be established.
    ///
    /// A staleness bound is a promise about age, and `05_blocking-decisions.md`
    /// §C-05 decided that routing **excludes** a node beyond the bound rather
    /// than serving it with a marker — *a marker nobody is obliged to read is
    /// not a guarantee*. Excluding needs an age to compare, and this is the only
    /// one this build can honestly produce.
    ///
    /// # Currency here is an identity, not a measurement
    ///
    /// A node whose effective roles carry `writable` is the origin of the data
    /// it holds: there is nothing for it to be stale *relative to*, so its copy
    /// is current as of now. Reading [`Self::effective_roles`] rather than the
    /// adopted set is what makes that keep being true — a leader whose lease has
    /// lapsed stops being current in the same instant it stops being writable,
    /// which is the answer that is true, because from that moment somebody else
    /// may be taking writes it has not seen.
    ///
    /// # Unknown is outside every bound
    ///
    /// A node that may not write holds a copy of somebody else's writes, and
    /// nothing in this build can say how old that copy is. There is no follower
    /// loop: `Session::replicate_from` is the door on the **leader's** side and
    /// no part of this process pulls through it, so a collected copy has no last
    /// collection to be measured from. `None` is therefore the honest answer and
    /// a caller must treat it as beyond every bound — which refuses something
    /// that might have been fine rather than serving something that might not
    /// be, the same direction [`crate::Lease`] errs.
    ///
    /// # Unknown until it has been level, and that is not the same as unknown
    /// until it has collected
    ///
    /// A node that may not write now answers the time since it was last
    /// **level** with the peer it collects from — see [`crate::Collections`] for
    /// why a collection that filled its bound proves only that this node asked.
    /// A node that has collected and never arrived still answers `None`, because
    /// its copy has no age anybody can state.
    ///
    /// It is the age of the last *arrival* and not of the data, so it is a lower
    /// bound: the leader may have written since. That is the same caveat
    /// [`crate::FollowerLag::quiet_for`] carries on the other side, and closing
    /// it needs a time in the log, which is Q-542's.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when the node
    /// identity cannot be read.
    pub fn current_as_of(&self) -> Result<Option<std::time::Duration>> {
        if self.effective_roles()?.has(Roles::WRITABLE) {
            return Ok(Some(std::time::Duration::ZERO));
        }
        Ok(self
            .collections
            .last()
            .and_then(|collection| collection.level_at)
            .map(|level| level.elapsed()))
    }

    /// Adopt the role the cluster wants this node to have.
    ///
    /// The **effective** role of `04_concept.md` §6.1 moving toward the
    /// **desired** one. A membership row bound to this node's id says what it is
    /// supposed to be; `META` says what it currently is; this closes the gap.
    ///
    /// Answers the roles it adopted, or `None` when it adopted nothing — which
    /// is both of the ordinary cases: no row names this node, or one does and
    /// the two already agree.
    ///
    /// # Why here, and why only here
    ///
    /// *The panel assigns, the node reconciles* (§C-19). Opening the store is
    /// the node's own reconcile point: it is the moment the process has a
    /// catalog to read and has not yet answered anybody, so the role it serves
    /// under is the role it settled on rather than one that changed underneath a
    /// request. A node converging **while running** would need a watch over the
    /// replicated row, and nothing watches it: the peer cadences pull records
    /// and exchange greetings, but no path re-adopts a role after open. So the
    /// window in which desired and effective differ is, for now, exactly the
    /// span between a declaration and the next open. That window is the thing
    /// S5.2 asks to be observable, and it is.
    ///
    /// It follows that `DEFINE NODE ROLES` on a **bound** node is an override
    /// the next open discards. That is C-19's decision showing through rather
    /// than an accident: a node's role is shared truth, and a local word that
    /// outlived the shared one would be the split-brain this whole section
    /// exists to prevent, in miniature.
    ///
    /// # A bound row with no roles drains the node
    ///
    /// An absent `ROLES` clause already means [`Roles::NONE`], which already
    /// means *takes no writes* — and `Roles::NONE` is documented as how an
    /// operator drains a node without stopping it. Applied to this node the same
    /// value keeps the same meaning, so binding a row and saying nothing about
    /// roles drains it at the next open. That is a sharp edge and it is the
    /// price of one value meaning one thing; the alternative is a second
    /// spelling for absent, and two spellings for absent disagree.
    ///
    /// # A membership row this build cannot read now refuses the open
    ///
    /// New, and deliberate. Before this, an unreadable row broke `INFO FOR NODE`
    /// and a forward; now it stops the store opening at all, because the
    /// question it makes unanswerable is *what is this node allowed to be*. The
    /// two ways to be wrong are not symmetric: refusing is an outage an operator
    /// sees immediately, and carrying on means running under a role the cluster
    /// may not have given — which is a node accepting writes it was supposed to
    /// forward, silently, which is the failure `roles` exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when a stored
    /// membership row or the node identity cannot be read.
    pub(super) fn reconcile_roles(&self) -> Result<Option<Roles>> {
        let identity = self.node_identity()?;
        let mut transaction = self.begin()?;
        let desired = crate::catalog::Catalog::new(&mut transaction).desired_roles(&identity.id)?;
        let Some(desired) = desired else {
            return Ok(None);
        };
        if desired == identity.roles {
            return Ok(None);
        }
        self.configure_node(Some(desired), None, None)?;
        Ok(Some(desired))
    }
}

/// Who may write one range, as the write gate judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Led {
    /// No leadership row covers it on its own line: nobody is fenced off it.
    Unled,
    /// This node leads it.
    Here,
    /// The range admits a second writer (`MULTI MASTER`), so nobody is fenced
    /// off it — the gate passes over it as over an unled one.
    Shared,
    /// Another node leads it.
    Elsewhere(crate::catalog::LeadershipDefinition),
}

impl Store {
    /// The node leading `range` when it is not this one, judged as the write
    /// gate judges it; `None` when this node may write it.
    ///
    /// # Errors
    ///
    /// The substrate's failure, and a decoding failure when a stored definition
    /// cannot be read.
    pub fn leader_of(&self, range: Reach) -> Result<Option<[u8; NODE_ID_LEN]>> {
        let me = self.node_identity()?.id;
        let mut transaction = self.begin()?;
        let catalog = crate::catalog::Catalog::new(&mut transaction);
        let held = catalog.leaderships()?;
        let placed: BTreeSet<Reach> = catalog
            .replicas()?
            .into_iter()
            .filter_map(|peer| peer.leads)
            .collect();
        drop(transaction);
        Ok(match self.led(&held, &placed, range, &me)? {
            Led::Elsewhere(leader) => Some(leader.node),
            Led::Here | Led::Unled | Led::Shared => None,
        })
    }

    /// Whether this node leads `range`, judged as the write gate judges it — or
    /// stands alone, naming no peer, where nothing is led and it writes
    /// everything. A clustered range nobody leads yet is not this node's.
    ///
    /// # Errors
    ///
    /// The substrate's failure, and a decoding failure when a stored definition
    /// cannot be read.
    pub fn leads(&self, range: Reach) -> Result<bool> {
        let me = self.node_identity()?.id;
        let mut transaction = self.begin()?;
        let catalog = crate::catalog::Catalog::new(&mut transaction);
        let held = catalog.leaderships()?;
        let peers = catalog.replicas()?;
        drop(transaction);
        let placed: BTreeSet<Reach> = peers.iter().filter_map(|peer| peer.leads).collect();
        Ok(match self.led(&held, &placed, range, &me)? {
            Led::Here => true,
            Led::Unled => peers.is_empty(),
            Led::Elsewhere(_) | Led::Shared => false,
        })
    }

    /// Who leads `range`, judged exactly as the write gate judges it — the one
    /// answer [`Self::refuse_if_led_elsewhere`] and a transaction across
    /// leaders both read, so the two cannot come to disagree about a range.
    pub(crate) fn led(
        &self,
        held: &[crate::catalog::LeadershipDefinition],
        placed: &BTreeSet<Reach>,
        range: Reach,
        me: &[u8; NODE_ID_LEN],
    ) -> Result<Led> {
        let Some(leader) = crate::catalog::covering(held, range) else {
            return Ok(Led::Unled);
        };
        // A row on another line says nothing about this range (Q-858): a
        // placement carves its range out of every coarser line, so the
        // store leader's row — the only one a placed range's first leader
        // holds until its own win is recorded — must not send it elsewhere.
        let line = crate::catalog::governing(placed, range);
        if crate::catalog::governing(placed, leader.range) != line {
            return Ok(Led::Unled);
        }
        // Per line (ADR-0082): a row's epoch is on the line its range is
        // governed by, and only this node's epoch on that same line orders
        // against it. Two lines' epochs are two unrelated counters.
        let mine = self.leading_of(crate::catalog::governing(placed, leader.range));
        if leader.node == *me || mine.is_some_and(|mine| mine > leader.epoch) {
            return Ok(Led::Here);
        }
        // G027 S2.3 — asked HERE, on the range that is about to be refused,
        // and not in front of the loop. A range declared `MULTI MASTER` has
        // no single leader to be writing *elsewhere* from: the row naming
        // another node is a second master, which is what the declaration
        // says the range admits. Reading it costs a catalog lookup, so it is
        // paid only by a write that was otherwise going to be redirected —
        // a leader writing its own range never reaches this line, because
        // `leader.node == *me` sent it back round.
        //
        // Per range and not once for the transaction, because a transaction
        // touching a declared range and an undeclared one must still be
        // refused for the undeclared one. Exempting on the first offender
        // would let the second travel under its cover.
        if self.admits_two_writers(range)? {
            return Ok(Led::Shared);
        }
        Ok(Led::Elsewhere(*leader))
    }
}
