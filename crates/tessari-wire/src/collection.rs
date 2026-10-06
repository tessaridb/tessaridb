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
mod serving;
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
pub use serving::Serving;
pub use stream::{Following, PUSHED_FROM, Pushed, StreamAsk, Streamed};
pub(crate) use stream::{
    answer as stream_answer, answer_pushed as stream_answer_pushed, held as stream_held,
};

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

    /// [`Self::collected`] for a round the leader pushes (ADR-0120 D1): the
    /// same answer, recorded as sent and never as held.
    ///
    /// Defaults to refusing, the safe direction: the only other answer a door
    /// that forgot it could give is `collected`'s, which counts the round as held
    /// and so acknowledges a write by a copy that is not durable.
    ///
    /// # Errors
    ///
    /// As [`Self::collected`].
    fn collected_pushed(&self, _follower: [u8; NODE_ID_LEN], _asked: Collect) -> Result<Collected> {
        Err(Error::Unsubscribed)
    }

    /// Record that `follower` holds `asked.home` durably up to just before
    /// `asked.from` — the acknowledgement an ask used to carry (ADR-0120 D2).
    ///
    /// Defaults to recording nothing, the safe direction: no write is ever
    /// acknowledged by a report that was not counted.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn held(&self, _follower: [u8; NODE_ID_LEN], _asked: Collect) -> Result<()> {
        Ok(())
    }

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

#[cfg(test)]
mod tests;
