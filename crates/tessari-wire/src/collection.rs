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

use std::net::SocketAddr;

use rustls::pki_types::CertificateDer;

use tessari_constants::{COLLECTION_BUDGET_BYTES, COLLECTION_PAGE_RECORDS};
use tessari_encoding::{LogId, LogRecord, NODE_ID_LEN, StoreValue};
use tessari_storage::{Catalog, Currency, Reach, Store};
use tessari_types::{Epoch, Sequence};

use crate::error::{Error, Result};
use crate::frame;
use crate::link::{Answered, Ask, Credential, call};
use crate::peer::Hello;

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
        Ok(Self {
            log,
            previous: Epoch::new(previous),
            records,
            stopped_early,
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
}

impl<'a> Serving<'a> {
    /// Serve `log` to the peers `log`'s own catalog subscribed.
    #[must_use]
    pub fn declared(log: &'a Store) -> Self {
        Self {
            log,
            granted: log,
            budget: COLLECTION_BUDGET_BYTES,
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
        }
    }
}

impl Origin for Serving<'_> {
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
        let served = self.log.own_log(asked.home).map_err(|why| Error::Refused {
            message: why.to_string(),
        })?;
        let previous = preceding(self.log, over, served, asked.from)?;
        // A `u64` from a peer against a `usize` here: on a platform where the
        // two differ the ask is larger than anything this node could answer, so
        // the whole log is the honest ceiling.
        let limit = usize::try_from(asked.limit).unwrap_or(usize::MAX);
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
        Ok(Collected {
            log: served,
            previous,
            records,
            stopped_early,
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
            let page = self
                .log
                .log_records_within(over, log, cursor, room.min(COLLECTION_PAGE_RECORDS))
                .map_err(refused)?;
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
    let held = store
        .log_records_within(over, log, before, 1)
        .map_err(|why| Error::Refused {
            message: why.to_string(),
        })?;
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

        let log = collected.log;
        let carried = u64::try_from(collected.records.len()).unwrap_or(u64::MAX);
        let mut previous = collected.previous;
        let mut reached = held;
        for (at, record) in &collected.records {
            // The log this collect read, which is the log it is applied into.
            // The two were allowed to differ while the frame named none: a
            // namespace subscriber read the leader's namespace log and filed
            // every record in its OWN store log, so the sequences counted in a
            // counter they never came from and nothing was in an error state to
            // say so.
            into.apply_from_stream(log, *at, previous, record)
                .map_err(refused)?;
            previous = record.epoch();
            reached = *at;
        }

        // Short means the peer had no more, which is the one moment this node
        // can observe that its copy was current. A full answer is contact and
        // not arrival, and recording it as arrival would admit exactly the read
        // a staleness bound exists to exclude.
        //
        // Short is no longer enough on its own. A leader fills one answer under
        // a byte budget as well as a record count, so an answer can be short
        // because the leader had no more OR because the answer was full — and
        // only the leader knows which. It says so, and a follower that read
        // *level* off the budget would record itself current while it is
        // behind, which is precisely the reading the bound exists to exclude.
        let currency = if carried < self.limit && !collected.stopped_early {
            Currency::Level
        } else {
            Currency::Behind
        };
        into.collected(reached, currency);
        Ok(reached)
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
                for span in shards.spans() {
                    logs.push(Reach::Shard(namespace.id, database.id, table.id, span.id));
                }
            }
        }
    }
    transaction.rollback();
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
fn refuse_to_reinterpret(into: &Store) -> Result<()> {
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
fn refused(why: tessari_storage::Error) -> Error {
    Error::Refused {
        message: why.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COLLECTION_BUDGET_BYTES, Catalog, Collect, Collected, Collector, LogId, NODE_ID_LEN, Reach,
        Result, Serving, StoreValue, logs_to_collect,
    };
    use tessari_types::{DatabaseId, NamespaceId};

    use crate::error::Error;
    use crate::grant::Deciding;
    use crate::link::tests::{Authority, THERE, hello, settled};
    use crate::link::{Answered, Ask, Peers, call};
    use crate::peer::Purpose;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tessari_encoding::{LogRecord, Writer};
    use tessari_types::{Epoch, Sequence};
    use tessaridb::Db;

    /// The node every door in this module belongs to.
    const LEADER: [u8; NODE_ID_LEN] = [70_u8; NODE_ID_LEN];

    /// A cluster in which everybody is subscribed to everything.
    ///
    /// The transfer is what this module's tests are about — that a batch
    /// applies in order, that a chain refuses a substituted record, that a short
    /// answer means level — and every one of them would otherwise have to
    /// declare a peer in a catalog to say so. What the catalog actually answers
    /// is tested where it is decided, against a real declaration.
    #[derive(Debug)]
    struct Everything;

    impl super::Subscriptions for Everything {
        fn granted(&self, _follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
            Ok(Some(Reach::Store))
        }
    }

    /// A store holding one empty record per epoch named, at 1, 2, 3…
    ///
    /// Empty records because what is being tested is the transfer and the
    /// leadership it states, and a mutation would only make the assertions
    /// longer. The epochs are what a run of leaderships actually looks like in
    /// the log, and a commit path in this build cannot produce them — it writes
    /// every record under [`Epoch::ZERO`], which is exactly the value that makes
    /// *the epoch at this position* and *the leader's latest epoch* impossible
    /// to tell apart.
    fn logged(epochs: &[u64]) -> Arc<Db> {
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        let writer = db.store().writer().expect("an identity");
        logging(&db, writer, epochs);
        db
    }

    /// The same, in the log `writer` allocates into.
    ///
    /// A follower's copy of a leader's log is filed under the LEADER's name, so
    /// a fixture that stands a follower part-way through one has to say whose
    /// log it is standing in. Seeding it under the follower's own name builds a
    /// second log that the collect below never reads, and the symptom is a gap
    /// at position one rather than the disagreement the test is about.
    fn logged_as(writer: Writer, epochs: &[u64]) -> Arc<Db> {
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        logging(&db, writer, epochs);
        db
    }

    /// Apply one empty record per epoch, into `writer`'s log.
    fn logging(db: &Arc<Db>, writer: Writer, epochs: &[u64]) {
        for (index, epoch) in epochs.iter().enumerate() {
            let at = Sequence::new(
                u64::try_from(index)
                    .expect("a handful of records")
                    .saturating_add(1),
            );
            db.store()
                .apply_record(writer, at, &LogRecord::at(Epoch::new(*epoch), Vec::new()))
                .expect("an empty record applies at the next position");
        }
    }

    /// The store's own log, as the node that wrote it names it.
    fn store_log(db: &Arc<Db>) -> LogId {
        db.store()
            .own_log(Reach::Store)
            .expect("the store's own identity")
    }

    /// What one empty record costs in the answer, measured rather than assumed.
    ///
    /// The budget is in bytes, so a test that hard-coded a size would be
    /// asserting today's encoding instead of the bound.
    fn one_record() -> usize {
        LogRecord::at(Epoch::new(1), Vec::new()).encode().len()
    }

    #[test]
    fn a_collection_stops_at_the_byte_budget_and_says_that_it_did() {
        let db = logged(&[1, 1, 1, 1, 1, 1]);
        // Room for two records and not the third, against a follower asking for
        // the whole log — which is the ask nothing caps.
        let serving = Serving::within(db.store(), &Everything, one_record() * 2);
        let (records, stopped_early) = serving
            .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
            .expect("a store-reach read of its own log");

        assert_eq!(
            records.len(),
            2,
            "the budget bounds the answer, not the ask"
        );
        assert!(
            stopped_early,
            "an answer the budget cut short says so, because the receiver cannot tell"
        );
    }

    #[test]
    fn the_answer_after_a_budgeted_one_resumes_where_it_stopped() {
        let db = logged(&[1, 1, 1, 1, 1, 1]);
        let serving = Serving::within(db.store(), &Everything, one_record() * 2);
        let (first, _) = serving
            .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
            .expect("a store-reach read of its own log");
        let next = Sequence::new(
            first
                .last()
                .expect("the first answer carried records")
                .0
                .get()
                .saturating_add(1),
        );

        // There is no continuation state on the leader — the cursor is a value
        // the collector holds — so resuming is the same call from a later
        // position, which is what a follower actually does.
        let roomy = Serving::within(db.store(), &Everything, one_record() * 64);
        let (second, stopped_early) = roomy
            .fill(Reach::Store, store_log(&db), next, usize::MAX)
            .expect("a store-reach read of its own log");

        assert_eq!(second.len(), 4, "the rest of the log, and none of it twice");
        assert_eq!(second.first().expect("records").0, Sequence::new(3));
        assert!(
            !stopped_early,
            "the second answer reached the end of the log and did not stop early"
        );
    }

    #[test]
    fn a_record_that_alone_exceeds_the_budget_is_still_carried() {
        let db = logged(&[1, 1, 1]);
        let serving = Serving::within(db.store(), &Everything, 0);
        let (records, stopped_early) = serving
            .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
            .expect("a store-reach read of its own log");

        // A budget that could answer nothing would leave a follower asking for
        // the same position forever, which is worse than the frame it costs.
        assert_eq!(records.len(), 1, "the first record is carried regardless");
        assert!(stopped_early);
    }

    #[test]
    fn an_answer_that_reached_the_end_of_the_log_did_not_stop_early() {
        let db = logged(&[1, 1, 1]);
        let serving = Serving::within(db.store(), &Everything, one_record() * 64);
        let (records, stopped_early) = serving
            .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
            .expect("a store-reach read of its own log");

        assert_eq!(records.len(), 3);
        assert!(
            !stopped_early,
            "*you are level* and *the budget filled* are opposite instructions"
        );
    }

    #[test]
    fn a_follower_whose_leader_stopped_on_the_budget_does_not_record_itself_current() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        // Room for one record against a log of three, so the leader stops on
        // bytes with the follower's own record count nowhere near full.
        let (address, door) = serving_within(&authority, &leader, 1, one_record());

        let follower = Db::in_memory().expect("an in-memory store");
        // A node that may not write, because a writable one answers zero by
        // identity and would prove nothing here.
        follower.hold_lease(Duration::ZERO);
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);

        // A generous count: the answer comes back short of it, which is the
        // reading that used to mean *the peer had no more* and now does not.
        collector(&mine, &der, &said, address, 64)
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("the collection");
        door.join().expect("the door's thread");

        assert_eq!(
            follower
                .store()
                .current_as_of()
                .expect("a store can be asked"),
            None,
            "an answer the budget cut short is contact, not arrival — a follower \
             that read it as arrival would report a copy as current while it is \
             three records behind, and *current* is what a staleness bound reads"
        );
    }

    /// S3.2 — the refusal, and the only thing that lifts it.
    ///
    /// The unit half of ADR-0078. A live run asserts the same two words reach an
    /// operator through a real node's log; this asserts the decision itself, in
    /// one process and without a cadence, so the message can be changed with a
    /// test that fails in milliseconds rather than in ninety seconds.
    ///
    /// The second half is the criterion's *destructive path exercised
    /// separately*, and it is deliberately the SAME test: a refusal nothing can
    /// lift is an outage, and a lift asserted apart from the refusal would pass
    /// on a build where the two conditions had drifted apart.
    #[test]
    fn a_node_holding_a_tenancy_of_its_own_is_refused_until_it_removes_it() {
        let authority = Authority::new();
        let leader = granting(" REPLICATES STORE");
        // One, and that is an assertion in itself: the refusal spends NO
        // connection, because it is taken before the dial. A door serving two
        // would hang on an accept that never happens.
        let (address, door) = declaring_for(&authority, &leader, 1);

        let follower = Db::in_memory().expect("an in-memory store");
        follower
            .session()
            .run("DEFINE NAMESPACE research;")
            .expect("a node that holds something of its own");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let collector = collector(&mine, &der, &said, address, 64);

        let refused = collector
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect_err("a node that holds a tenancy of its own may not collect");
        let words = refused.to_string();
        assert!(
            words.contains("research"),
            "the refusal names what would be reinterpreted, or an operator \
             cannot act on it: {words}"
        );
        assert!(
            words.contains("DROP NAMESPACE"),
            "and names the statement that lifts it: {words}"
        );

        // Refused means nothing arrived, not that the failure was reported after
        // the fact — which is the whole difference between this and a warning.
        // Read from the catalog rather than through `USE NAMESPACE`, which sets
        // the session's context without asking whether the namespace is there.
        assert_eq!(
            declared(&follower),
            vec!["research".to_owned()],
            "the cluster's namespaces must not have been applied"
        );

        // The destruction, stated by being performed. Nothing authorises it on
        // the joiner's behalf and nothing outlives it.
        follower
            .session()
            .run("DROP NAMESPACE research;")
            .expect("the operator removes their own tenancy");
        collector
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("a node with no tenancy of its own has nothing to reinterpret");
        door.join().expect("the door's thread");

        assert_eq!(
            declared(&follower),
            vec!["prod".to_owned(), "other".to_owned()],
            "and then the cluster's namespaces arrive, under the ids the \
             cluster gave them"
        );
    }

    /// Every namespace a store can name, in catalog order.
    fn declared(db: &Db) -> Vec<String> {
        let mut transaction = db.store().begin().expect("a read");
        let names = Catalog::new(&mut transaction)
            .namespaces()
            .expect("the catalog answers")
            .into_iter()
            .map(|namespace| namespace.name)
            .collect();
        transaction.rollback();
        names
    }

    /// A leader whose catalog actually grants, built by running statements.
    ///
    /// The other helper in this module applies log records directly, which is
    /// the right shape for a test about the transfer and the wrong one here: a
    /// subscription is a catalog record, so the only honest way to have one is
    /// to have declared it.
    fn granting(clause: &str) -> Arc<Db> {
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        db.session()
            .run(&format!(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                 USE DATABASE orders; DEFINE COLLECTION users; \
                 CREATE users:1 = {{ name: 'ada' }}; \
                 DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE ledger; \
                 USE DATABASE ledger; DEFINE COLLECTION secrets; \
                 CREATE secrets:1 = {{ word: 'shibboleth' }}; \
                 DEFINE REPLICA follower AT '127.0.0.1:1' NODE '{}'{clause};",
                spelled(THERE)
            ))
            .expect("the leader's own statements run");
        db
    }

    /// A node id as a `NODE` clause takes it: thirty-two hex digits.
    fn spelled(node: [u8; NODE_ID_LEN]) -> String {
        node.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A peer door for `LEADER` serving one connection, asking `db`'s own
    /// catalog who may collect.
    fn declaring(authority: &Authority, db: &Arc<Db>) -> (SocketAddr, JoinHandle<()>) {
        declaring_for(authority, db, 1)
    }

    /// The same door, serving `rounds` connections.
    ///
    /// A collection is one connection, and a follower walking the chain from the
    /// store's log down to its own reach makes one per log — so a test about the
    /// chain has to say how many, and has to then make exactly that many or the
    /// join waits on an accept nobody ever performs.
    fn declaring_for(
        authority: &Authority,
        db: &Arc<Db>,
        rounds: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(LEADER, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let mine = hello(LEADER);
        let db = Arc::clone(db);
        let door = std::thread::spawn(move || {
            for _ in 0..rounds {
                drop(peers.greet(
                    || Ok(mine),
                    &LEADER,
                    &Deciding::holding(settled()),
                    &Serving::declared(db.store()),
                ));
            }
        });
        (address, door)
    }

    /// A peer door for `LEADER` that serves `rounds` connections out of `db`.
    fn serving(authority: &Authority, db: &Arc<Db>, rounds: usize) -> (SocketAddr, JoinHandle<()>) {
        serving_within(authority, db, rounds, COLLECTION_BUDGET_BYTES)
    }

    /// The same door, serving under a byte budget a test can actually reach.
    fn serving_within(
        authority: &Authority,
        db: &Arc<Db>,
        rounds: usize,
        budget: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(LEADER, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let mine = hello(LEADER);
        let db = Arc::clone(db);
        let door = std::thread::spawn(move || {
            for _ in 0..rounds {
                drop(peers.greet(
                    || Ok(mine),
                    &LEADER,
                    &Deciding::holding(settled()),
                    &Serving::within(db.store(), &Everything, budget),
                ));
            }
        });
        (address, door)
    }

    /// Ask the door at `address` for the store log's records after `from`.
    fn collect(
        authority: &Authority,
        address: SocketAddr,
        from: u64,
        limit: u64,
    ) -> crate::error::Result<Answered> {
        collect_from(authority, address, Reach::Store, from, limit)
    }

    /// The same, naming the log.
    fn collect_from(
        authority: &Authority,
        address: SocketAddr,
        home: Reach,
        from: u64,
        limit: u64,
    ) -> crate::error::Result<Answered> {
        Ok(call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            LEADER,
            &hello(THERE),
            Ask::Records(Collect {
                home,
                from: Sequence::new(from),
                limit,
            }),
        )?
        .1)
    }

    /// The records in an answer, or `None` when the peer answered otherwise.
    ///
    /// An `Option` and not a panicking unwrap because the workspace denies
    /// panicking paths, and `.expect` at the call site says what was expected
    /// in the same place the assertion about it lives.
    fn served(answered: Answered) -> Option<Collected> {
        match answered {
            Answered::Collected(collected) => Some(collected),
            _ => None,
        }
    }

    #[test]
    fn a_collection_frame_round_trips() {
        let asked = Collect {
            home: Reach::Database(NamespaceId::new(4), DatabaseId::new(9)),
            from: Sequence::new(7),
            limit: 64,
        };
        assert_eq!(Collect::decode(&asked.encode()).expect("an ask"), asked);
        // The range is the field a number means nothing without, so it
        // round-trips at every level rather than at the one the fixture happened
        // to pick.
        for home in [
            Reach::Store,
            Reach::Namespace(NamespaceId::new(1)),
            Reach::Database(NamespaceId::new(1), DatabaseId::new(2)),
        ] {
            let asked = Collect {
                home,
                from: Sequence::new(1),
                limit: 8,
            };
            assert_eq!(Collect::decode(&asked.encode()).expect("an ask").home, home);
        }

        let answer = Collected {
            log: LogId::unattributed(Reach::Store),
            previous: Epoch::new(3),
            records: vec![
                (Sequence::new(7), LogRecord::at(Epoch::new(4), Vec::new())),
                (Sequence::new(8), LogRecord::at(Epoch::new(4), Vec::new())),
            ],
            stopped_early: true,
        };
        let back = Collected::decode(&answer.encode()).expect("an answer");
        assert_eq!(back, answer);
        // `true` above and `false` here, because a flag that survived a round
        // trip in one state only would pass a test written with either.
        let full = Collected {
            stopped_early: false,
            ..answer.clone()
        };
        assert_eq!(
            Collected::decode(&full.encode()).expect("an answer"),
            full,
            "the flag travels in both states"
        );
        // A body from a leader with no budget carries no flag at all, and reads
        // as the thing such a leader always was: never stopped early.
        let mut older = full.encode();
        older.pop();
        assert_eq!(
            Collected::decode(&older).expect("an answer with no flag"),
            full,
            "a body with no flag reads as an answer that did not stop early"
        );
        // The leadership before the batch travels separately from the ones
        // inside it, and they differ here on purpose: a codec that carried one
        // of them twice would pass a test where they were equal.
        assert_eq!(back.previous, Epoch::new(3));
    }

    /// The wire is the first of S1.3's three carriers, and it carries the stamp
    /// without knowing it exists.
    ///
    /// That is the point rather than an accident of this test: the frame carries
    /// a log record's encoded bytes, and the stamp lives inside a mutation's
    /// value, so nothing in this crate had to change for a stamped record to
    /// cross a link. The assertion below is what turns that from a claim into
    /// evidence — it compares the bytes AND reads the stamp back out, because a
    /// frame that dropped the flag bit would still produce equal-looking records
    /// if only the payload were compared.
    #[test]
    fn a_stamped_record_survives_the_collection_frame() {
        use tessari_encoding::{CausalStamp, Mutation, RecordValue, StampedValue};
        use tessari_types::{RecordId, TableId};

        let mut stamp = CausalStamp::new();
        stamp.advance([1_u8; NODE_ID_LEN]);
        stamp.advance([1_u8; NODE_ID_LEN]);
        stamp.advance([2_u8; NODE_ID_LEN]);

        let record = LogRecord::at(
            Epoch::new(4),
            vec![Mutation {
                namespace: NamespaceId::new(1),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("contested"),
                shard: None,
                value: StampedValue::stamped(
                    stamp.clone(),
                    RecordValue::Present(b"from one of two masters".to_vec()),
                ),
            }],
        );
        let answer = Collected {
            log: LogId::unattributed(Reach::Store),
            previous: Epoch::new(3),
            records: vec![(Sequence::new(7), record)],
            stopped_early: false,
        };

        let encoded = answer.encode();
        let back = Collected::decode(&encoded).expect("an answer");
        assert_eq!(back, answer, "the frame did not carry the record unchanged");
        assert_eq!(
            back.encode(),
            encoded,
            "re-encoding what came off the wire did not reproduce the same bytes"
        );
        let carried = &back.records[0].1.mutations()[0].value;
        assert_eq!(carried.stamp(), &stamp, "the causal stamp did not survive");
        assert_eq!(carried.stamp().count(&[1_u8; NODE_ID_LEN]), 2);
        assert_eq!(carried.stamp().count(&[2_u8; NODE_ID_LEN]), 1);
    }

    #[test]
    fn a_follower_collects_the_records_it_does_not_have() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 1);

        let collected = served(
            collect(&authority, address, 1, 64).expect("a peer that proved itself may collect"),
        )
        .expect("a collection came back");
        door.join().expect("the door's thread");

        assert_eq!(
            collected
                .records
                .iter()
                .map(|(at, _)| at.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the whole log, in order, from the position asked for"
        );
        // Nothing precedes the first position, and the answer says so with the
        // value a receiver would compute for itself.
        assert_eq!(collected.previous, Epoch::ZERO);
    }

    #[test]
    fn a_collection_states_the_epoch_that_precedes_it() {
        let authority = Authority::new();
        // Three leaderships, one record each. Asking from 3 makes the answer
        // *2* while the leader's latest is *3* — which is the only arrangement
        // in which reading the epoch at the position and reading the store's
        // latest give different answers.
        let leader = logged(&[1, 2, 3]);
        let (address, door) = serving(&authority, &leader, 1);

        let collected =
            served(collect(&authority, address, 3, 64).expect("the door is up and answering"))
                .expect("a collection came back");
        door.join().expect("the door's thread");

        assert_eq!(
            collected.previous,
            Epoch::new(2),
            "the leadership at sequence 2, not the one at the tail"
        );
        assert_eq!(collected.records.len(), 1, "one record stands after 2");
    }

    #[test]
    fn a_follower_that_asks_beyond_the_leaders_log_is_refused() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 2);

        // Level is not the same as beyond. A follower holding all three asks
        // from 4, the leader holds 3, and the honest answer is an EMPTY
        // collection — which is what makes the refusal below about the
        // position rather than about emptiness.
        let level = served(collect(&authority, address, 4, 64).expect("a level follower may ask"))
            .expect("a collection came back");
        assert!(level.records.is_empty(), "{:?}", level.records);
        assert_eq!(level.previous, Epoch::new(1), "the leadership at the tail");

        let refused = collect(&authority, address, 9, 64);
        door.join().expect("the door's thread");

        // And it crossed the wire as the refusal it was, not as a closed
        // socket: `from` is the position that was asked for.
        assert!(
            matches!(refused, Err(Error::Uncollectable { from: 9 })),
            "expected the position to be refused by name, got {refused:?}"
        );
    }

    #[test]
    fn a_limit_bounds_one_collection_and_the_next_asks_from_where_it_stopped() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 2);

        let first = served(collect(&authority, address, 1, 2).expect("the first collection"))
            .expect("a collection came back");
        assert_eq!(
            first
                .records
                .iter()
                .map(|(at, _)| at.get())
                .collect::<Vec<_>>(),
            vec![1, 2],
            "the bound the follower named, and not the whole log"
        );

        // No continuation state on the leader: the follower's cursor is the
        // position it reached, and the next ask is an ordinary one.
        let next = served(collect(&authority, address, 3, 2).expect("the second collection"))
            .expect("a collection came back");
        door.join().expect("the door's thread");
        assert_eq!(
            next.records
                .iter()
                .map(|(at, _)| at.get())
                .collect::<Vec<_>>(),
            vec![3],
            "the rest, starting where the first collection stopped"
        );
    }

    #[test]
    fn a_collection_is_recorded_on_the_leader() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 1);

        assert!(
            leader
                .store()
                .follower_lag()
                .expect("a leader can be asked")
                .is_empty(),
            "nothing has collected yet"
        );

        drop(collect(&authority, address, 1, 2).expect("a collection"));
        door.join().expect("the door's thread");

        let lag = leader
            .store()
            .follower_lag()
            .expect("a leader can be asked");
        assert_eq!(lag.len(), 1, "{lag:?}");
        let seen = lag.first().expect("one follower");
        // By the id the HANDSHAKE proved, and at the position it was actually
        // handed — which is 2 and not 3, because the bound stopped the answer
        // short and the leader records what it gave rather than what it holds.
        assert_eq!(seen.node, THERE);
        assert_eq!(seen.sequence, Sequence::new(2));
        assert_eq!(seen.behind, 1, "one commit stands beyond what it was given");
    }

    /// A follower that collects from `address`, with a bound of `limit`.
    fn collector<'a>(
        mine: &'a crate::link::Credential,
        der: &'a rustls::pki_types::CertificateDer<'a>,
        said: &'a crate::peer::Hello,
        address: SocketAddr,
        limit: u64,
    ) -> Collector<'a> {
        Collector {
            mine,
            authority: der,
            said,
            peer: (LEADER, address),
            limit,
        }
    }

    #[test]
    fn a_node_nobody_subscribed_is_refused_the_log_it_asks_for() {
        // C-37's own cheapest decisive test, and the low-privilege probe the
        // access-control discipline asks for: a node with a credential this
        // cluster issued, proven at the door, asking directly over the protocol
        // with nothing else in the loop.
        let authority = Authority::new();
        let leader = granting("");
        let (address, door) = declaring(&authority, &leader);

        let refused = collect(&authority, address, 1, 64)
            .expect_err("a peer nobody subscribed may not take the log");
        door.join().expect("the door's thread");

        // The refusal it was, not a closed socket: a node whose connection
        // ended mid-frame would be looking for a network fault instead of
        // reading the one sentence that says what to do.
        assert!(
            matches!(refused, Error::Unsubscribed),
            "a refusal, and not the same one a stranded follower gets: {refused}"
        );
        let said = refused.to_string();
        assert!(
            said.contains("DEFINE REPLICA") && said.contains("REPLICATES"),
            "and it names the statement that grants one: {said}"
        );
    }

    #[test]
    fn a_split_tables_shards_are_logs_a_follower_asks_for_after_its_database() {
        let db = Db::in_memory().expect("an in-memory store");
        db.session()
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT 'g';",
            )
            .expect("a split table");
        let logs = logs_to_collect(db.store()).expect("this node's own logs");
        let at = logs
            .iter()
            .position(|home| matches!(home, Reach::Database(..)))
            .expect("the database is a log");
        let after: Vec<Option<u32>> = logs[at + 1..]
            .iter()
            .map(|home| match home {
                Reach::Shard(_, _, _, shard) => Some(shard.get()),
                _ => None,
            })
            .collect();
        let shards: Vec<u32> = after.iter().flatten().copied().collect();
        assert_eq!(
            after.len(),
            shards.len(),
            "only the table's shards follow its database: {logs:?}"
        );
        assert_eq!(shards, vec![1, 2]);
    }

    #[test]
    fn a_subscriber_receives_its_namespace_and_not_the_one_beside_it() {
        /// One per level of the reach lattice, which is what bounds the chain.
        const ROUNDS: usize = 3;

        let authority = Authority::new();
        let leader = granting(" REPLICATES NAMESPACE prod");
        // One round per log the follower asks for, and it discovers the logs as
        // it goes: the store's own first, then the namespace that arrived in it.
        let (address, door) = declaring_for(&authority, &leader, ROUNDS);

        let follower = Db::in_memory().expect("an in-memory store");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let collector = collector(&mine, &der, &said, address, 1024);

        // The chain, walked the way the node's own loop walks it: collect a
        // log, then re-derive the set, because the namespace whose log is worth
        // asking for only exists once the store's log has been applied.
        let mut reached = Sequence::ZERO;
        let mut asked: Vec<Reach> = Vec::new();
        for _ in 0..ROUNDS {
            let logs = logs_to_collect(follower.store()).expect("this node's own logs");
            // The next log this node has not asked for, or the store's own
            // again. Never a `break`: the door accepts exactly `ROUNDS`
            // connections, and stopping short leaves it waiting on one that
            // never arrives.
            let home = logs
                .into_iter()
                .find(|home| !asked.contains(home))
                .unwrap_or(Reach::Store);
            asked.push(home);
            // Each round asks a log this node has not asked for, so the first
            // position it does not hold there is the first one there is. Reading
            // its own tail would be the node's own loop and not this test's: the
            // raw feed is not something a test in a networked crate reaches
            // either, and `enforcement.rs` is right to say so.
            reached = collector
                .collect(follower.store(), home, Sequence::new(1))
                .expect("a subscribed peer collects");
        }
        door.join().expect("the door's thread");
        assert!(reached.get() >= 1, "the leader had a log to hand over");
        assert!(
            asked.contains(&Reach::Store),
            "the store's own log is where the namespace definition lives"
        );
        assert!(
            asked.iter().any(|home| matches!(home, Reach::Namespace(_))),
            "and the namespace's own log is where its records live: {asked:?}"
        );

        // Read back through a session rather than through the log, because what
        // the subscription is *for* is which records exist on the follower — and
        // a log comparison would pass on a build that transferred the bytes and
        // applied none of them.
        let mut session = follower.session();
        let held = session
            .run("USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;")
            .expect("the subscribed namespace arrived");
        assert_eq!(held.len(), 3, "three statements, three outcomes");

        let missing = session
            .run("USE NAMESPACE other; USE DATABASE ledger; SELECT * FROM secrets;")
            .expect_err("the namespace beside the subscription must not have arrived");
        assert!(
            missing.to_string().contains("no namespace named \"other\""),
            "the namespace beside the subscription never arrived, and the \
             refusal names it: {missing}"
        );
    }

    #[test]
    fn a_record_read_out_of_one_log_is_applied_into_that_same_log() {
        // The defect this slice closes, asserted by POSITION and not by
        // presence: a follower used to read the leader's namespace log and file
        // every record in its own store log, so the records arrived and the
        // sequences counted in a counter they never came from.
        const ROUNDS: usize = 2;

        let authority = Authority::new();
        let leader = granting(" REPLICATES NAMESPACE prod");
        let (address, door) = declaring_for(&authority, &leader, ROUNDS);

        let follower = Db::in_memory().expect("an in-memory store");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let collector = collector(&mine, &der, &said, address, 1024);

        // The log the subscribed namespace's records actually live in. It is
        // the DATABASE's and not the namespace's: a record homes at the join of
        // the reaches its mutations carried to, and a `CREATE` inside one
        // database joins to that database. The leader holds no namespace-level
        // log at all, which is worth knowing before writing an assertion about
        // one — `leader.store().logs()` answers it and compiles nothing.
        let inside = leader
            .store()
            .logs()
            .expect("the leader's own logs")
            .into_iter()
            .find(|log| matches!(log.home, Reach::Database(namespace, _) if namespace.get() == 1))
            .expect("the fixture writes records inside prod's database");
        for home in [Reach::Store, inside.home] {
            collector
                .collect(follower.store(), home, Sequence::new(1))
                .expect("a subscribed peer collects");
        }
        door.join().expect("the door's thread");

        // Positions rather than a count, because the defect being closed put the
        // right records in the wrong counter: a follower that folded them into
        // its store log would hold every record and no position in this one.
        let at = |db: &Db, log| -> Vec<Sequence> {
            db.store()
                .log_records(log, Sequence::ZERO, 64)
                .expect("a log reads back")
                .into_iter()
                .map(|(sequence, _)| sequence)
                .collect()
        };
        let theirs = at(&leader, inside);
        assert!(
            !theirs.is_empty(),
            "the fixture must put records in that log for this to assert anything"
        );
        assert_eq!(
            at(&follower, inside),
            theirs,
            "the records were read out of that log and must count in the \
             follower's copy of it, at the same positions"
        );
        assert!(
            follower
                .store()
                .logs()
                .expect("the follower's logs")
                .contains(&inside),
            "and the log must exist on the follower rather than its records \
             having been folded into the store's"
        );
    }

    #[test]
    fn a_log_no_subscription_reaches_is_refused_rather_than_served() {
        let authority = Authority::new();
        let leader = granting(" REPLICATES NAMESPACE prod");
        let (address, door) = declaring(&authority, &leader);

        let beside = logs_to_collect(leader.store())
            .expect("the leader's own logs")
            .into_iter()
            .rfind(|home| matches!(home, Reach::Namespace(_)))
            .expect("the fixture declares two namespaces");
        let refused = collect_from(&authority, address, beside, 1, 64)
            .expect_err("a namespace this peer is not subscribed to");
        door.join().expect("the door's thread");

        // The same refusal a peer nobody subscribed gets, and deliberately so:
        // the repair is the same `REPLICATES` clause, and a fourth frame would
        // send an operator to it by a different sentence.
        assert!(
            matches!(refused, Error::Unsubscribed),
            "a log outside the grant is a refusal and not an empty answer: {refused}"
        );
        let said = refused.to_string();
        assert!(
            said.contains("reaches the log it asked for"),
            "and the sentence is true of a partial subscription too: {said}"
        );
    }

    #[test]
    fn a_bounded_read_this_node_could_not_answer_becomes_answerable_once_it_collects() {
        // What the whole wave is for, stated as the one observable that changed.
        // Before this build a follower's `current_as_of` could only ever be
        // `None` — there was no code path by which a node that may not write
        // became level with anything — so every bounded read on every follower
        // was refused, and *this node is too far behind* could not be told apart
        // from *this node has never heard from anybody*.
        let authority = Authority::new();
        let leader = granting(" REPLICATES STORE");
        let (address, door) = declaring(&authority, &leader);

        let follower = Db::in_memory().expect("an in-memory store");
        // First, and it is not interchangeable with the lines that follow: a
        // node that may not write may not define anything either, so the role
        // has to be taken before there is a schema — which here there never is,
        // because the schema arrives by collection.
        follower
            .session()
            .run("DEFINE NODE ROLES serving;")
            .expect("a node may say what it is for");
        assert_eq!(
            follower
                .store()
                .current_as_of()
                .expect("a store can say how old its copy is"),
            None,
            "a node that has collected nothing has no known age, which is \
             outside every bound rather than inside the ones nobody measured"
        );

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let reached = collector(&mine, &der, &said, address, 1024)
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("a subscribed peer collects");
        door.join().expect("the door's thread");
        assert!(reached.get() > 1, "the leader had a log to hand over");

        // A short answer is the one moment a follower can observe that its copy
        // was current: the peer served fewer than the limit, so it had no more.
        let age = follower
            .store()
            .current_as_of()
            .expect("a store can say how old its copy is")
            .expect("a follower that collected to the end knows how old it is");
        assert!(
            age < std::time::Duration::from_secs(tessari_constants::STALENESS_FLOOR_SECONDS),
            "a copy that has just become level is inside the tightest bound the \
             API admits, and this one reads {age:?}"
        );

        // And the read that could not be answered before is answered now, by
        // this node, without leaving it.
        let answered = follower
            .session()
            .run(&format!(
                "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS {}s;",
                tessari_constants::STALENESS_FLOOR_SECONDS
            ))
            .expect("a copy inside the bound answers the read here");
        assert_eq!(answered.len(), 3, "three statements, three outcomes");
    }

    #[test]
    fn a_follower_that_collects_applies_what_it_was_given() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 1);

        let follower = Db::in_memory().expect("an in-memory store");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let reached = collector(&mine, &der, &said, address, 64)
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("a collection applies");
        door.join().expect("the door's thread");

        assert_eq!(reached, Sequence::new(3));
        assert_eq!(
            follower
                .store()
                .log_records(store_log(&leader), Sequence::new(1), 64)
                .expect("the log can be read")
                .len(),
            3,
            "the follower holds what it was given"
        );
    }

    #[test]
    fn a_batch_is_applied_against_the_record_before_each_one() {
        let authority = Authority::new();
        // Three leaderships in one batch. The answer states only what precedes
        // the FIRST record; if every record were applied against that same
        // epoch, the second would claim `Epoch::ZERO` stands at position 1 while
        // the follower has just written epoch 1 there, and the store would
        // refuse it as a divergence.
        let leader = logged(&[1, 2, 3]);
        let (address, door) = serving(&authority, &leader, 1);

        let follower = Db::in_memory().expect("an in-memory store");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let reached = collector(&mine, &der, &said, address, 64)
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("a batch of three leaderships applies");
        door.join().expect("the door's thread");

        assert_eq!(reached, Sequence::new(3));
    }

    #[test]
    fn a_collection_whose_predecessor_disagrees_is_refused() {
        let authority = Authority::new();
        // The two histories agree on how FAR they go and disagree on who wrote
        // it. Nothing about the offered record says so — the check is the
        // predecessor, which is why the frame carries one at all.
        let leader = logged(&[9, 9, 9]);
        let (address, door) = serving(&authority, &leader, 1);

        // Standing in the LEADER's log, two records in. That is where a
        // follower's copy of it lives, and it is the only place the histories
        // can disagree at all: two writers' logs are two counters, so a record
        // of one never lands at a position of the other.
        let follower = logged_as(leader.store().writer().expect("an identity"), &[1, 1]);
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let refused = collector(&mine, &der, &said, address, 64).collect(
            follower.store(),
            Reach::Store,
            Sequence::new(3),
        );
        door.join().expect("the door's thread");

        assert!(
            matches!(&refused, Err(Error::Refused { .. })),
            "expected the store's own refusal, got {refused:?}"
        );
        let message = match refused {
            Err(Error::Refused { message }) => message,
            _ => String::new(),
        };
        // The store's own words, carried through: a reworded divergence gives
        // an operator two accounts of one event.
        assert!(
            message.contains("epoch 1") && message.contains("epoch 9"),
            "{message}"
        );
        assert_eq!(
            follower
                .store()
                .log_records(store_log(&leader), Sequence::new(1), 64)
                .expect("the log can be read")
                .len(),
            2,
            "and nothing was appended"
        );
    }

    #[test]
    fn a_short_answer_tells_the_follower_how_old_its_copy_is() {
        let authority = Authority::new();
        let leader = logged(&[1, 1, 1]);
        let (address, door) = serving(&authority, &leader, 2);

        let follower = Db::in_memory().expect("an in-memory store");
        // A node that may not write: the half of `current_as_of` this wave is
        // about. A writable node answers zero by identity and would prove
        // nothing here.
        follower.hold_lease(Duration::ZERO);
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);

        // Bound of two against a log of three: the answer fills the bound, so
        // the follower asked and did not arrive.
        collector(&mine, &der, &said, address, 2)
            .collect(follower.store(), Reach::Store, Sequence::new(1))
            .expect("the first collection");
        assert_eq!(
            follower
                .store()
                .current_as_of()
                .expect("a store can be asked"),
            None,
            "a full answer is contact, not arrival"
        );

        // The rest arrives inside the bound, so the peer had no more.
        collector(&mine, &der, &said, address, 2)
            .collect(follower.store(), Reach::Store, Sequence::new(3))
            .expect("the second collection");
        door.join().expect("the door's thread");
        assert!(
            follower
                .store()
                .current_as_of()
                .expect("a store can be asked")
                .is_some(),
            "a short answer is the peer saying it had no more"
        );
    }
}
