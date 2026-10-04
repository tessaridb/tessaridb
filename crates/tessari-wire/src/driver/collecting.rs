use super::*;

/// How far this node has collected in each log, and what a failure does to it.
///
/// # One cursor per log, and not one cursor
///
/// A position counts in ONE log. A node holds one log per home, so a single
/// cursor carried across logs would advance in one space and be spent in
/// another — asking a namespace's log for a position the store's log had
/// reached, which is a gap or a re-send depending only on which log ran ahead.
#[derive(Debug, Default)]
pub struct Collecting {
    at: BTreeMap<Reach, Sequence>,
}

impl Collecting {
    /// A node that has collected nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            at: BTreeMap::new(),
        }
    }

    /// The position this node has reached in `home`, or `None` when it has not
    /// collected there at all.
    ///
    /// `None` rather than a zero, because *I have never collected this log* and
    /// *I collected it and reached the beginning* are different statements and
    /// the caller seeding a cursor has to tell them apart.
    #[must_use]
    pub fn reached(&self, home: Reach) -> Option<Sequence> {
        self.at.get(&home).copied()
    }

    /// One collection of one log. The cursor moves **only** when the pass
    /// answers.
    ///
    /// `seed` is where to start when this log has no cursor yet — ordinarily
    /// this node's own committed tail there plus one. It is read by the caller
    /// and not here, because a log this node has never collected is discovered
    /// by reading the catalog, and the rule `Collector::collect` documents keeps
    /// the feed out of this crate's reach.
    ///
    /// After a pass the cursor holds what the pass REACHED, which is the last
    /// position applied rather than the first one not held — so the next ask
    /// re-fetches one record. That is unchanged from when there was one cursor
    /// and is recorded rather than corrected here (Q-631).
    ///
    /// # Why a failure leaves the cursor alone
    ///
    /// A cursor advanced past records that were never applied skips them
    /// permanently and silently: the next pass asks for what comes after, no
    /// later pass ever asks for the gap, and nothing is in an error state to say
    /// so. Leaving it costs a repeated request when the peer comes back, which
    /// is the failure worth having.
    ///
    /// # Why the refusal is handed back rather than absorbed
    ///
    /// Until W382 this answered a `Sequence` either way, and the error was
    /// dropped at the one point in the program where it was in scope. The
    /// caller was then left with a cursor that had not moved — which is also
    /// what a healthy pass with nothing to fetch produces — so *the peer
    /// refused me* and *the peer had nothing for me* reached an operator as the
    /// same line, and a cluster replicating nothing looked exactly like a
    /// cluster that was already level.
    ///
    /// That is not a reporting nicety. It cost three processes, ninety seconds
    /// and a refuted hypothesis to learn that a follower was being turned away,
    /// because the sentence naming the reason existed only inside this function
    /// and was discarded here.
    ///
    /// # Errors
    ///
    /// Returns whatever `pass` returned. The cursor is already recorded when it
    /// does, so a caller that only wants to go on collecting may discard it —
    /// but it has to discard it deliberately.
    pub fn once<E>(
        &mut self,
        home: Reach,
        seed: Sequence,
        pass: impl FnOnce(Sequence) -> Result<Sequence, E>,
    ) -> Result<Sequence, E> {
        let at = self.at.get(&home).copied().unwrap_or(seed);
        match pass(at) {
            Ok(reached) => {
                self.at.insert(home, reached);
                Ok(reached)
            }
            Err(why) => {
                // The seed, when this log had no cursor at all: a failed first
                // pass still fixes where the next one starts, or the caller's
                // freshly-read tail would be handed to a retry as a new
                // beginning it has not earned.
                self.at.insert(home, at);
                Err(why)
            }
        }
    }
}

/// The peer this node collects from, if it should collect at all.
///
/// # A node that may write follows nobody
///
/// A node holding [`Roles::WRITABLE`] is the origin of what it holds — that is
/// exactly what `Store::current_as_of` says when it answers `Some(0)` — so it
/// has nothing to catch up to. Collecting into it would apply a peer's records
/// beside its own, which is the divergence the log's epoch chain exists to
/// refuse, discovered at apply time rather than prevented at the timer.
///
/// The roles are re-read every round rather than decided at start, so a node
/// that is told to stop writing begins following without being restarted.
///
/// # The catalog says who may be followed; the greeting says who to follow
///
/// ADR-0065. Until W255 this took the one peer the catalog declared `WRITABLE`,
/// which was correct while exactly one node could ever write. ADR-0063 and
/// ADR-0064 together make *every coordinating node also declared writable* the
/// configuration a cluster needs in order to fail over at all — and a rule that
/// reads the declaration then finds two writable rows and gives up, which is
/// what it did: `Error::ManyWritablePeers` once per collection interval, on
/// every node, forever, with nothing replicating and nothing in an error state
/// except a log line.
///
/// The leadership is a lease, so the answer moves at runtime and is not in a
/// row. It is already on the wire: [`crate::Hello::current_as_of`] is the
/// greeter's own `Store::current_as_of`, which answers `Some(0)` **exactly
/// when** its *effective* roles carry `WRITABLE` — a node that may write is the
/// origin of what it holds and has nothing to be stale relative to. So a
/// greeting says *I may write right now* as an effective fact rather than a
/// declared one, and [`crate::Hello::epoch`] says under which leadership.
///
/// The epoch breaks the tie, and the tie is not hypothetical: a leader demoted
/// a moment ago and its successor can both be in this directory, because a
/// greeting is as fresh as the last awareness round and no fresher. ADR-0059's
/// ordering picks the newer leadership, which is the same rule a voter applies
/// to a ballot.
///
/// Nothing is added to the wire and no new cadence is introduced — the
/// awareness round already fills this directory, and until now only the
/// staleness router read it.
///
/// # A peer declared without a node cannot be dialled
///
/// A peer connection demands a certificate valid for a name derived from the
/// peer's **id**, so an endpoint whose id nobody knows cannot be dialled at all
/// — the same wall the seed address runs into. `None` here rather than a
/// half-formed attempt: a row that says who but not where, or where but not
/// who, is a declaration the operator has not finished.
///
/// # A peer this node has never greeted is not followed
///
/// Absence of a greeting is not evidence that a peer may write, so a cold node
/// collects from nobody until its first awareness round has landed. One
/// interval of not collecting, against the alternative of pulling records from
/// whichever address happened to be declared first.
#[must_use]
pub fn upstream(
    mine: Roles,
    declared: &[ReplicaDefinition],
    heard: &Directory,
) -> Option<([u8; NODE_ID_LEN], String)> {
    if mine.has(Roles::WRITABLE) {
        return None;
    }
    declared
        .iter()
        .filter_map(|peer| Some((peer.node?, peer, heard.at(&peer.endpoint)?)))
        .filter(|(_, _, heard)| heard.said.current_as_of == Some(Duration::ZERO))
        .max_by_key(|(_, _, heard)| heard.said.epoch)
        .map(|(node, peer, _)| (node, peer.endpoint.clone()))
}

/// Where a node whose catalog names nobody collects from.
///
/// The bootstrap twin of [`upstream`], and every rule it applies is that
/// function's: a node that may write has no upstream, a greeting is required
/// before anything is followed, `current_as_of == Some(0)` is what *may write
/// right now* looks like on the wire, and the epoch breaks a tie between a
/// demoted leader and its successor. Only the candidate set differs — seeds
/// instead of declared peers — which is why the two are siblings rather than
/// one function with a flag: the set is the whole difference, and a flag would
/// invite a caller to pass both.
///
/// # Only while the catalog is empty
///
/// The caller uses this **instead of** [`upstream`] while the catalog declares
/// no peer, and never as a fallback when `upstream` happens to answer `None`.
/// The distinction matters and it is not stylistic. `upstream` answers `None`
/// for ordinary, temporary reasons — no greeting has landed yet, every declared
/// peer is currently a follower, this node may write — and a seed consulted on
/// any of those would be a node that stops believing its own membership the
/// moment the leader is briefly unreachable, and goes back to an address written
/// on a command line months ago. Empty is the only condition that means *this
/// node has not joined yet*.
///
/// # The seed is spent as soon as it works
///
/// `DEFINE REPLICA` is a catalog write and therefore already a log record, so
/// the membership arrives through the very collection this function starts.
/// After the first successful round the catalog names peers, the caller stops
/// asking this question, and nothing reads the seed again for the life of the
/// node. That is `04_concept.md` §6.3 holding exactly as written — *the seed
/// address is configuration, and everything after first contact lives in the
/// database* — and it needs no frame of its own to be true.
#[must_use]
pub fn bootstrap_from(
    mine: Roles,
    seeds: &[Seed],
    heard: &Directory,
) -> Option<([u8; NODE_ID_LEN], String)> {
    if mine.has(Roles::WRITABLE) {
        return None;
    }
    seeds
        .iter()
        .filter_map(|seed| Some((seed, heard.at(&seed.endpoint)?)))
        .filter(|(_, heard)| heard.said.current_as_of == Some(Duration::ZERO))
        .max_by_key(|(_, heard)| heard.said.epoch)
        .map(|(seed, _)| (seed.node, seed.endpoint.clone()))
}
