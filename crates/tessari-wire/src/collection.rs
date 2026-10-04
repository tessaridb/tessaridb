//! A follower asks for what it does not have, and a leader answers out of its
//! own log.
//!
//! # Why an answer carries a leadership the records do not
//!
//! A stream of bare records cannot tell a re-send from a divergence. Both arrive
//! at a position the receiver already holds, and until a record carried an epoch
//! the second writer's record was discarded in silence while two nodes drifted
//! apart reporting perfect health. [`Collected::previous`] is the other half of
//! that: the leadership that wrote the record **before** the first one carried
//! here, which is what lets the receiver check that the two histories are the
//! same one before it appends to it.
//!
//! It is read at `from - 1` and never from the leader's latest, because a
//! follower catching up legitimately replays records from leaderships that have
//! since ended — and every one of them would be a false divergence against the
//! latest.
//!
//! # What is not here
//!
//! **No timer and no thread.** [`Collector::collect`] performs ONE collection
//! and applies what comes back; nothing calls it on a clock. Deciding *when* to
//! collect belongs with the node's lifecycle, for the reason [`crate::Standing`]
//! gives about owning a clock — and the cursor travels with that decision, which
//! is why `collect` is told where to start rather than reading it here.
//!
//! **No bootstrap and no filtering.** A follower behind the retention floor
//! needs the log as a backup rather than as a stream, and a selective follower
//! needs the reach filter the store already has. Both are their own decisions;
//! this serves [`Reach::Store`] and refuses what it cannot state.

mod collector;
mod deposed;
mod stream;

use tessari_constants::{COLLECTION_BUDGET_BYTES, COLLECTION_PAGE_RECORDS};
use tessari_encoding::{LogId, LogRecord, NODE_ID_LEN, StoreValue};
use tessari_storage::{Catalog, Reach, Store};
use tessari_types::{Epoch, Sequence};

use crate::error::{Error, Result};
use crate::frame;
use crate::gathering::{Gather, Page, Ungathered};
pub(crate) use collector::refused;
pub use collector::{Collector, logs_to_collect};
pub(crate) use stream::answer as stream_answer;
pub use stream::{Following, StreamAsk, Streamed};

/// What a follower asks a leader for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collect {
    /// The log the cursor counts in.
    ///
    /// A position is a number in ONE log and means nothing in another, so an ask
    /// that named only a number was an ask the leader had to guess the space of
    /// — and it guessed from the grant, which is right for exactly one log and
    /// wrong for every other one the subscriber is entitled to (Q-620, Q-621).
    ///
    /// It is the follower's to name and not the leader's to derive, because a
    /// subscriber reads a CHAIN: the store's own log carries the namespace and
    /// database definitions its records depend on, and the grant names only the
    /// bottom of that chain. What the leader keeps is the authority to refuse —
    /// see [`Serving`], where a log the grant neither contains nor sits inside
    /// is answered with a refusal and not with records.
    pub home: Reach,
    /// The first position the follower does not hold — **inclusive**.
    ///
    /// It is also the follower's own assertion that it holds `from - 1`, which
    /// is what makes the answer's [`Collected::previous`] meaningful: the leader
    /// states the leadership at exactly that position, and the two either agree
    /// or the histories parted.
    pub from: Sequence,
    /// The most records one answer may carry.
    ///
    /// The follower asks again from where the answer stopped. There is no
    /// continuation state on the leader, for the same reason the client feed has
    /// none: the cursor is a value the collector holds, and the buffer is the
    /// log.
    pub limit: u64,
}

impl Collect {
    /// The body of a [`crate::PeerFrame::Collect`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(25);
        frame::put_reach(&mut body, self.home);
        frame::put_u64(&mut body, self.from.get());
        frame::put_u64(&mut body, self.limit);
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an ask takes.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (home, at) = frame::take_reach(body, 0)?;
        let (from, at) = frame::take_u64(body, at)?;
        let (limit, _) = frame::take_u64(body, at)?;
        Ok(Self {
            home,
            from: Sequence::new(from),
            limit,
        })
    }
}

/// What a leader answers a collection with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    /// The log these records were read out of.
    ///
    /// # Why the ANSWER names it and not the ask
    ///
    /// A follower asks for a range; a leader answers from the log it allocates
    /// into for that range, and since a range may admit two writers the home
    /// alone no longer picks one out. The follower cannot supply the missing
    /// half: the identity a peer is reached and verified BY is its credential's,
    /// and the identity it WRITES under is its store's — two values that agree
    /// in a deployment and are not the same field.
    ///
    /// So the leader states it, and the follower files what it was given where
    /// it was read from. That is the same sentence
    /// [`tessari_storage::Store::apply_record_in`] already makes about the home
    /// and for the same reason: a fact about the collect, not a second authority
    /// over the record.
    pub log: LogId,
    /// The leadership that wrote the record **before** the first one carried
    /// here, or [`Epoch::ZERO`] when the ask began at the first position and
    /// nothing precedes it.
    pub previous: Epoch,
    /// The records, in log order, beginning at the position that was asked for.
    ///
    /// Empty is a real answer and means *you are level* — which is exactly why
    /// a leader that cannot serve the position at all answers a different frame
    /// rather than an empty one.
    pub records: Vec<(Sequence, LogRecord)>,
    /// The leader had more to give and stopped because this answer was full.
    ///
    /// # It is not an optimisation and the receiver cannot infer it
    ///
    /// A collector reads *level* off a short answer: fewer records than it asked
    /// for meant the leader had no more. A byte budget breaks that inference,
    /// because a short answer now means either *you are level* or *the budget
    /// filled* — and those are opposite instructions to a follower. Without this
    /// field a follower whose leader stopped on bytes would record itself
    /// current while it is behind, and *current* is what a staleness bound reads
    /// before admitting the node to a read.
    ///
    /// A body that does not carry it reads `false`, which is the right answer
    /// rather than a default: a leader with no budget never stopped early.
    pub stopped_early: bool,
    /// The reach this answer was served under — the follower's grant, as the
    /// leader applied it (G031, ADR-0081).
    ///
    /// A fact about the collect, for the reason [`Self::log`] is one: the
    /// follower records it and uses it only to NARROW — which logs it asks for,
    /// and which reads it will answer — and the leader keeps refusing every log
    /// outside the grant whatever the follower recorded. A body from a leader
    /// that predates the field carries nothing here, which reads as not stated:
    /// the follower then holds everything it has, as every follower did before.
    pub over: Option<Reach>,
    /// The writer's commit order this leader had reached when it read the
    /// records (G034, ADR-0084).
    ///
    /// Read BEFORE the records, so every commit of this log ordered at or below
    /// it is in the answer when the answer is level. A follower applying several
    /// of one writer's logs in commit order needs exactly that: a level page
    /// with no order proves nothing about what the log may yet hold below a
    /// record of another log. A body from a leader that predates the field
    /// carries nothing here, and the follower then applies as it always did.
    pub order: Option<Sequence>,
    /// The leadership `order` counts under (ADR-0107), read with it.
    ///
    /// Each leader that continues a single-leader range's log stamps its OWN
    /// counter, so an order means nothing without the leadership it was reached
    /// under: a follower bounding a round by it would otherwise hold back every
    /// record an earlier leader stamped higher. Written after `order` and only
    /// with it; an answer without it is read at the newest leadership the round
    /// holds, which is what a leader that predates it implies.
    pub epoch: Option<Epoch>,
}

impl Collected {
    /// The body of a [`crate::PeerFrame::Collected`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_log(&mut body, self.log);
        frame::put_u64(&mut body, self.previous.get());
        frame::put_u64(
            &mut body,
            u64::try_from(self.records.len()).unwrap_or(u64::MAX),
        );
        for (at, record) in &self.records {
            frame::put_u64(&mut body, at.get());
            // The store's own encoding, unchanged. A second codec for the same
            // record is a second thing that has to stay true across a version.
            frame::put_bytes(&mut body, record.encode().as_slice());
        }
        body.push(u8::from(self.stopped_early));
        // A tail field after the last one a previous build wrote, so an older
        // follower stops reading before it and a newer one finds nothing there
        // in an older leader's answer.
        if let Some(over) = self.over {
            frame::put_reach(&mut body, over);
            // After `over` and only with it, so its position is known.
            if let Some(order) = self.order {
                frame::put_u64(&mut body, order.get());
                if let Some(epoch) = self.epoch {
                    frame::put_u64(&mut body, epoch.get());
                }
            }
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an answer
    /// takes, and the encoding's own failure when a record cannot be decoded.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (log, at) = frame::take_log(body, 0)?;
        let (previous, at) = frame::take_u64(body, at)?;
        let (count, at) = frame::take_u64(body, at)?;
        // Deliberately not `with_capacity(count)`: the count came from the other
        // end, and a reader that allocated whatever it was told would be one
        // frame away from being out of memory. The body is already bounded by
        // the frame ceiling, so growing as records actually arrive costs nothing
        // that matters and cannot be driven.
        let mut records = Vec::new();
        let mut at = at;
        for _ in 0..count {
            let (sequence, next) = frame::take_u64(body, at)?;
            let (bytes, next) = frame::take_bytes(body, next)?;
            at = next;
            records.push((Sequence::new(sequence), LogRecord::decode(&bytes)?));
        }
        // Absent reads `false` — see the field. A body from a leader that has no
        // budget carries nothing here and never stopped early, so the missing
        // byte and the byte it would have written say the same thing.
        let stopped_early = body.get(at).is_some_and(|flag| *flag != 0);
        let after = at.saturating_add(1);
        let (over, order, epoch) = if body.len() > after {
            let (over, next) = frame::take_reach(body, after)?;
            let (order, next) = if body.len() > next {
                let (order, next) = frame::take_u64(body, next)?;
                (Some(Sequence::new(order)), next)
            } else {
                (None, next)
            };
            let epoch = if order.is_some() && body.len() > next {
                Some(Epoch::new(frame::take_u64(body, next)?.0))
            } else {
                None
            };
            (Some(over), order, epoch)
        } else {
            (None, None, None)
        };
        Ok(Self {
            log,
            previous: Epoch::new(previous),
            records,
            stopped_early,
            over,
            order,
            epoch,
        })
    }
}

/// What a peer door may do to the log it serves from.
///
/// One method, and that is the point. The door is handed this rather than a
/// store, so what a peer connection can reach is a matter of what this trait
/// says rather than of what the door's author remembered not to call. It also
/// means a test about a handshake needs a handshake and not a storage engine.
pub trait Origin {
    /// Answer `asked` for the follower that asked it, and record the collection.
    ///
    /// Recording is part of the same call because the door is the only way
    /// through: a peer read that goes unrecorded is then not expressible, which
    /// is the argument [`tessari_session::Session::replicate_from`] was built on
    /// one layer up.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Uncollectable`] when the leadership before `asked.from`
    /// cannot be stated, and [`Error::Refused`] carrying the store's own words
    /// when the log cannot be read.
    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected>;

    /// Answer a gather of one shard's records for the peer that asked (G033).
    ///
    /// Beside collection because it is the same door answering the same peer
    /// out of the same store, and asked of the same catalog who may have what.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotGathered`] with the reason when the peer may not have
    /// the shard or this node cannot give it.
    fn gathered(&self, asker: [u8; NODE_ID_LEN], asked: &Gather) -> Result<Page>;

    /// Stream this node's state, as the follower's subscription is given it,
    /// through `write` — a head, the chunks, an end (ADR-0094 D3).
    ///
    /// No default: a door that forgot this would refuse every copy as
    /// unsubscribed and still compile, which is how the node's own door first
    /// shipped it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsubscribed`] when nothing grants the follower a reach,
    /// the store's own failure, and whatever `write` returns.
    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()>;

    /// Whether this node's catalog places `candidate` on `range` — what a
    /// voter asks before granting a ballot on a placed range's line (ADR-0098).
    ///
    /// No default, for [`Self::copied`]'s reason: a door that forgot it would
    /// grant every range ballot.
    fn places(&self, candidate: [u8; NODE_ID_LEN], range: Reach) -> bool;

    /// Bind `node` to the row waiting on the join token `token`, and answer
    /// whether a row is now bound to it (ADR-0108 D9).
    ///
    /// Defaults to binding nothing, the safe direction: a door that forgot it
    /// leaves a joiner waiting rather than admitting anybody.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn joined(&self, _node: [u8; NODE_ID_LEN], _token: &[u8; 32]) -> Result<bool> {
        Ok(false)
    }

    /// Where this node's own copy of `range`'s line reaches, read from what it
    /// holds — the position a voter judges a ballot for that range against.
    ///
    /// Not from this node's greeting, which describes only the one range the
    /// node is placed on: a former leader, or a follower that collected the
    /// line, holds the line's log while its greeting says zero there, and a
    /// voter that believed the greeting granted a candidate behind it — whose
    /// leadership then continued the line from its shorter tail and left the
    /// records only the voter held behind (Q-884).
    ///
    /// Defaults to `None`, meaning *ask the greeting*: a door with no log has
    /// nothing better to answer with.
    ///
    /// # Errors
    ///
    /// A store failure, which refuses the vote rather than guessing a position.
    fn reached_on(&self, _range: Reach) -> Result<Option<crate::grant::Reached>> {
        Ok(None)
    }

    /// Answer a peer's sign-in try from this store's failure table — the
    /// cluster's while this node leads the store line (ADR-0108 D5).
    ///
    /// Defaults to `false`, the safe direction for [`Self::joined`]'s reason: a
    /// door with no store behind it makes a name wait rather than letting a
    /// guess through uncounted.
    fn attempted(&self, _asked: &crate::budget::Attempt) -> bool {
        false
    }
}

/// A door with no log behind it.
///
/// Two callers, one reason. **The peer door** answers three things today: who is
/// there, how a ballot goes, and how far a log reaches. The fourth — handing
/// over the records themselves — is an *authorization* question and not a wiring
/// one: a follower's reach is the one its subscription grant gave it
/// (`Reach::Namespace` for a selective follower), while [`Origin`] as
/// implemented for [`Store`] answers at `Reach::Store`, which is every tenant's
/// records regardless of what any grant said. Wiring that into the door would
/// hand every proven peer the whole store, so until the grant reaches the door
/// the door serves no log. **A test about a handshake** wants the same thing for
/// a cheaper reason: making it build a storage engine would put an engine in the
/// path of a test about a greeting.
///
/// It refuses rather than answering empty, because *nothing to give* and *you
/// are level* must never look alike (see [`Error::Uncollectable`]) — and it
/// refuses as `Uncollectable` rather than by ending the conversation, which is
/// the difference between a follower learning *not from here* and a follower
/// watching its socket close mid-frame and reading it as a network fault.
///
/// This goes the day the subscription grant reaches the peer door. It is not a
/// placeholder for that work: it is the honest answer while the door cannot ask
/// the question.
#[derive(Debug, Clone, Copy)]
pub struct NoLog;

impl Origin for NoLog {
    fn collected(&self, _follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        Err(Error::Uncollectable {
            from: asked.from.get(),
        })
    }

    fn gathered(&self, _asker: [u8; NODE_ID_LEN], _asked: &Gather) -> Result<Page> {
        Err(Error::NotGathered(Ungathered::NotHeld))
    }

    // A door with no catalog cannot vouch for any placement, so it grants no
    // range ballot.
    fn places(&self, _candidate: [u8; NODE_ID_LEN], _range: Reach) -> bool {
        false
    }

    // A door with no log has no state to give, which is the refusal a node
    // nobody subscribed gets.
    fn copied(
        &self,
        _follower: [u8; NODE_ID_LEN],
        _write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        Err(Error::Unsubscribed)
    }
}

/// Who may collect this store's log, and how much of it.
///
/// A trait rather than a catalog read inlined into the door, for the reason
/// [`NoLog`] exists: a test about the transfer would otherwise have to build a
/// catalog to prove that a batch applies in order. The production answer has one
/// implementation, on [`Store`], and it is the catalog — so the door cannot be
/// given a second opinion by a call site.
pub trait Subscriptions: core::fmt::Debug {
    /// How far `follower` may collect, or `None` when it may not at all.
    ///
    /// `None` is the refusal and it is the default: it is what a peer nobody
    /// subscribed answers, and what every peer declared before subscriptions
    /// existed answers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] when the answer cannot be read.
    fn granted(&self, follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>>;
}

/// The catalog's answer, which is the only one that governs.
///
/// Looked up by the id the peer certificate proved rather than by the name an
/// operator wrote: a name is a word every node can read, so a subscription
/// written against one would be held by whichever node answered to it. A row
/// with no `NODE` therefore matches nobody, which is what makes every peer
/// declaration written before this existed grant nothing.
impl Subscriptions for Store {
    fn granted(&self, follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
        let mut transaction = self.begin().map_err(refused)?;
        let declared = Catalog::new(&mut transaction).replicas();
        transaction.rollback();
        Ok(declared
            .map_err(refused)?
            .into_iter()
            .find(|row| row.node == Some(follower))
            .and_then(|row| row.replicates))
    }
}

/// A log, served to exactly the peers something says may have it.
///
/// # Why the reach is not an argument on the wire
///
/// The subscription answers both halves of the question — whether this node may
/// take the log at all, and how much of it it then receives — and it answers
/// them as **one value**. A design in which the follower named a reach and the
/// door checked it against a grant has a state in which a peer authorized for
/// one namespace is served another, with neither call wrong about its own
/// argument. There is no such state here, because only one reach is ever named.
///
/// # Why there is no unsubscribed way to serve a log
///
/// Until this wave the implementation of [`Origin`] was on [`Store`] itself and
/// served [`Reach::Store`] unconditionally, which is why no door was ever given
/// one: it would hand every proven peer every tenancy's records, credential
/// hashes included. That implementation is gone rather than kept beside this
/// one, so default-deny is a property of what this crate can express rather
/// than a rule a future wiring has to remember.
#[derive(Debug)]
pub struct Serving<'a> {
    /// The log itself.
    log: &'a Store,
    /// Who may have it.
    granted: &'a dyn Subscriptions,
    /// The most bytes of records one answer carries.
    ///
    /// A field rather than a constant read at the point of use, for the reason
    /// [`Self::asking`] is a constructor: a bound nothing can afford to exercise
    /// is a bound nothing checks, and forcing this one at its real value costs
    /// four megabytes of log per assertion.
    budget: usize,
    /// The most records one page of groups folds, for the same reason: at its
    /// real value a second page costs 65 536 records (ADR-0097 D2).
    fold_records: usize,
}

impl<'a> Serving<'a> {
    /// Serve `log` to the peers `log`'s own catalog subscribed.
    #[must_use]
    pub fn declared(log: &'a Store) -> Self {
        Self {
            log,
            granted: log,
            budget: COLLECTION_BUDGET_BYTES,
            fold_records: tessari_constants::GATHER_FOLD_RECORDS,
        }
    }

    /// Serve `log`, asking `granted` who may have it.
    ///
    /// The seam a test uses, and the reason it is here rather than in a test
    /// module: a test about whether a batch applies in order should not have to
    /// declare a peer to find out.
    #[must_use]
    pub fn asking(log: &'a Store, granted: &'a dyn Subscriptions) -> Self {
        Self {
            log,
            granted,
            budget: COLLECTION_BUDGET_BYTES,
            fold_records: tessari_constants::GATHER_FOLD_RECORDS,
        }
    }

    /// Serve `log` with a byte budget of `budget` rather than the standard one.
    ///
    /// The second seam, and the same argument as the first: the consequence of
    /// an answer stopping early is what a follower records about how current its
    /// copy is, and a test that cannot make a leader stop early cannot observe
    /// that consequence at all.
    #[must_use]
    pub fn within(log: &'a Store, granted: &'a dyn Subscriptions, budget: usize) -> Self {
        Self {
            log,
            granted,
            budget,
            fold_records: tessari_constants::GATHER_FOLD_RECORDS,
        }
    }

    /// The same, folding at most `records` records into one page of groups.
    #[cfg(test)]
    pub(crate) const fn folding_by(mut self, records: usize) -> Self {
        self.fold_records = records;
        self
    }
}

impl Origin for Serving<'_> {
    fn attempted(&self, asked: &crate::budget::Attempt) -> bool {
        asked.answered(self.log)
    }

    // The line's one history as this store holds it (ADR-0107): its tail and
    // the leadership that wrote it, the pair a greeting carries for its own line.
    fn reached_on(&self, range: Reach) -> Result<Option<crate::grant::Reached>> {
        let refused = |why: tessari_storage::Error| Error::Refused {
            message: why.to_string(),
        };
        let log = self.log.history_log(range).map_err(refused)?;
        Ok(Some(crate::grant::Reached {
            leadership: self.log.tail_leadership(log).map_err(refused)?,
            tail: self.log.committed_tail(log).map_err(refused)?,
        }))
    }

    // The rule a candidate stands by (`campaign_line`), read from this node's
    // own catalog — and for a range being given back to the store line, the
    // store line's leader as this catalog records it (ADR-0098 D3). A catalog
    // that cannot be read vouches for nothing.
    fn places(&self, candidate: [u8; NODE_ID_LEN], range: Reach) -> bool {
        let Ok(mut transaction) = self.log.begin() else {
            return false;
        };
        let catalog = Catalog::new(&mut transaction);
        let read = catalog
            .replicas()
            .and_then(|declared| Ok((declared, catalog.leader_of(Reach::Store)?)));
        transaction.rollback();
        read.is_ok_and(|(declared, store)| {
            let leads_the_store = store.is_some_and(|leader| leader.node == candidate);
            crate::campaign_line(&declared, &candidate, leads_the_store) == Some(range)
        })
    }

    fn gathered(&self, asker: [u8; NODE_ID_LEN], asked: &Gather) -> Result<Page> {
        crate::gathering::serve(
            self.log,
            self.granted,
            asker,
            asked,
            (self.budget, self.fold_records),
        )
    }

    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let Some(over) = self.granted.granted(follower)? else {
            return Err(Error::Unsubscribed);
        };
        crate::copying::serve(self.log, over, write)
    }

    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        let Some(over) = self.granted.granted(follower)? else {
            // Not `Uncollectable`: *you may not ask* and *I cannot state what
            // precedes your position* send an operator to two different people,
            // one holding a `DEFINE REPLICA` and the other a backup.
            return Err(Error::Unsubscribed);
        };
        // A log the grant neither contains nor sits inside holds nothing this
        // follower is entitled to. Both directions are servable and they serve
        // different halves of the chain: a log INSIDE the grant is entirely the
        // follower's, and a log ABOVE it — the store's own, for a namespace
        // subscriber — is read with the mutations outside the reach elided,
        // which is what carries `DEFINE NAMESPACE` to a subscriber without
        // carrying the namespace beside it.
        //
        // The refusal is `Unsubscribed` rather than a fourth frame because the
        // repair is the same statement: a `REPLICATES` that does not cover the
        // log asked for. A follower that derives its log set from the catalog it
        // has itself replayed never produces it — see [`crate::logs_to_collect`]
        // — so this answers a peer that is out of step or out of order, and the
        // one thing it must not do is answer it with somebody else's records.
        if !(over.contains(asked.home) || asked.home.contains(over)) {
            return Err(Error::Unsubscribed);
        }
        // The log this leader allocates into for the range asked for. The ask
        // names the range and the answer names the log, because the follower
        // cannot name the writer: the identity a peer is verified BY is its
        // credential's and the identity it WRITES under is its store's.
        // The line's one log of a single-leader range once it holds records
        // (ADR-0107), which every leader continues; before any leadership this
        // node's own, as a cluster without elections replicates; the leader's
        // own where two may write.
        let served = self
            .log
            .history_log(asked.home)
            .map_err(|why| Error::Refused {
                message: why.to_string(),
            })?;
        let previous = preceding(self.log, over, served, asked.from)?;
        // The ask is the acknowledgement (ADR-0106 D6): a follower asks for the
        // first position it does not hold, once it has applied and synced what
        // it was sent. Counted only as far as this leader sent it — see
        // `tessari_storage::Store::follower_asked`.
        self.log.follower_asked(
            follower,
            served,
            Sequence::new(asked.from.get().saturating_sub(1)),
        );
        // A position below where this log now begins cannot be caught up from
        // the log: it is the answer to *copy my state*, not to *catch me up*
        // (ADR-0094 D3), and it crosses as the frame that already says so.
        // A `u64` from a peer against a `usize` here: on a platform where the
        // two differ the ask is larger than anything this node could answer, so
        // the whole log is the honest ceiling.
        let limit = usize::try_from(asked.limit).unwrap_or(usize::MAX);
        // Before the records, so a commit landing while they are read is above
        // it rather than missing below it (ADR-0084).
        let order = self.log.committed_version().map_err(refused)?;
        // And the leadership that order counts under, read with it (ADR-0107) —
        // stated only by a node that leads the line, since only a leader's next
        // commit is ordered by its own counter; one that leads nothing commits
        // nothing here, and says so by stating no leadership.
        let epoch = self.log.writing_epoch(asked.home).map_err(refused)?;
        let epoch = (epoch > Epoch::ZERO).then_some(epoch);
        let (records, stopped_early) = self.fill(over, served, asked.from, limit)?;
        // What the follower now holds: the last position it was handed, or —
        // when it was handed nothing — the one it told us it was at. The same
        // rule the leader's own door uses, because it is the same event.
        let reached = records.last().map_or_else(
            || Sequence::new(asked.from.get().saturating_sub(1)),
            |(sequence, _)| *sequence,
        );
        // The log the collect read, which is the one `reached` counts in — the
        // follower's ask, because a position recorded against any other log is
        // two unrelated counters subtracted (Q-630).
        self.log.follower_served(follower, asked.home, reached);
        // Only records sent count toward what a later ask may vouch for — a
        // level answer sends nothing, and a copy that is level on its own word
        // has not been checked against this leader's history.
        if let Some((last, _)) = records.last() {
            self.log.follower_sent(follower, served, *last);
        }
        Ok(Collected {
            log: served,
            previous,
            records,
            stopped_early,
            over: Some(over),
            order: Some(order),
            epoch,
        })
    }
}

impl Serving<'_> {
    /// Fill one answer under both bounds and say whether more was waiting.
    ///
    /// # Why the log is read a page at a time
    ///
    /// The follower's `limit` is a record count and nothing caps what it may
    /// name, so an ask for the whole log would otherwise be one read of the
    /// whole log — and the frame writer's ceiling, the only thing that refuses
    /// today, refuses after the reading has already happened. A budget can only
    /// be honoured by a read that stops, so this reads a page, measures what it
    /// has, and asks for another page only while there is room.
    ///
    /// The budget is bytes because records differ in size by orders of
    /// magnitude: any record count either throttles a follower carrying small
    /// commits or fails to protect against one carrying large ones.
    ///
    /// The first record is always carried, even when it alone exceeds the
    /// budget. A budget that could return nothing would leave a follower asking
    /// for the same position forever, which is worse than the frame this node
    /// then has to build.
    ///
    /// The budget is [`Serving`]'s own field so that a test can observe the
    /// bound without writing four megabytes to reach it — see [`Serving::within`].
    fn fill(
        &self,
        over: Reach,
        log: LogId,
        from: Sequence,
        limit: usize,
    ) -> Result<(Vec<(Sequence, LogRecord)>, bool)> {
        let mut carried: Vec<(Sequence, LogRecord)> = Vec::new();
        let mut spent = 0_usize;
        let mut cursor = from;
        loop {
            let room = limit.saturating_sub(carried.len());
            if room == 0 {
                // The follower's own count is full. Whether more is waiting is
                // the question its next ask answers, and a count-full answer has
                // always meant *ask again*.
                return Ok((carried, false));
            }
            // Two reaches and they answer two different questions: `over` is
            // what this follower may SEE, and `log` is which log the cursor
            // counts in. They were one value while a frame carried no log, and
            // that made every ask a read of one link of the chain (Q-618).
            let page = match self.log.log_records_within(
                over,
                log,
                cursor,
                room.min(COLLECTION_PAGE_RECORDS),
            ) {
                Ok(page) => page,
                Err(tessari_storage::Error::BelowLogStart { .. }) => {
                    return Err(Error::Uncollectable { from: from.get() });
                }
                Err(why) => return Err(refused(why)),
            };
            if page.is_empty() {
                return Ok((carried, false));
            }
            let read = page.len();
            for (at, record) in page {
                // Measured as the answer will carry it, which is the encoding
                // the frame uses — a size taken from anywhere else is a second
                // account of one number.
                let size = record.encode().len();
                if !carried.is_empty() && spent.saturating_add(size) > self.budget {
                    return Ok((carried, true));
                }
                spent = spent.saturating_add(size);
                cursor = Sequence::new(at.get().saturating_add(1));
                carried.push((at, record));
            }
            if read < room.min(COLLECTION_PAGE_RECORDS) {
                // The page came back short, so the log had no more to give.
                return Ok((carried, false));
            }
        }
    }
}

/// The leadership that wrote the record before `from`.
///
/// # Errors
///
/// Returns [`Error::Uncollectable`] when this node holds nothing at `from - 1`.
fn preceding(store: &Store, over: Reach, log: LogId, from: Sequence) -> Result<Epoch> {
    if from.get() <= 1 {
        // Nothing precedes the first position, and a store that never elected
        // anybody writes exactly this epoch — so the answer is the same value a
        // receiver would compute for itself rather than a stand-in for one.
        return Ok(Epoch::ZERO);
    }
    let before = Sequence::new(from.get().saturating_sub(1));
    // At the follower's own reach and not the store's, which costs the answer
    // nothing: a scoped read keeps every sequence and removes only the mutations
    // outside the reach, so the record at this position and the epoch it carries
    // are the same value either way. Reading wider bought exactly the disclosure
    // the subscription exists to prevent, in the one place a reach was not
    // threaded through — which is how a rule acquires a hole.
    let held = match store.log_records_within(over, log, before, 1) {
        Ok(held) => held,
        Err(tessari_storage::Error::BelowLogStart { .. }) => {
            return Err(Error::Uncollectable { from: from.get() });
        }
        Err(why) => {
            return Err(Error::Refused {
                message: why.to_string(),
            });
        }
    };
    match held.first() {
        // The log answers from `before` ONWARD, so a position it no longer holds
        // comes back as the next one that does. Comparing the position is what
        // tells *the record before yours* apart from *some later record*, and
        // without it a follower behind the retention floor would be handed the
        // wrong leadership with every appearance of a correct answer.
        Some((at, record)) if *at == before => Ok(record.epoch()),
        _ => Err(Error::Uncollectable { from: from.get() }),
    }
}

#[cfg(test)]
mod tests;
