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

use tessari_encoding::{LogRecord, NODE_ID_LEN, StoreValue};
use tessari_storage::{Catalog, Currency, Reach, Store};
use tessari_types::{Epoch, Sequence};

use crate::error::{Error, Result};
use crate::frame;
use crate::link::{Answered, Ask, Credential, call};
use crate::peer::Hello;

/// What a follower asks a leader for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collect {
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
        let mut body = Vec::with_capacity(16);
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
        let (from, at) = frame::take_u64(body, 0)?;
        let (limit, _) = frame::take_u64(body, at)?;
        Ok(Self {
            from: Sequence::new(from),
            limit,
        })
    }
}

/// What a leader answers a collection with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
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
}

impl Collected {
    /// The body of a [`crate::PeerFrame::Collected`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
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
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an answer
    /// takes, and the encoding's own failure when a record cannot be decoded.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (previous, at) = frame::take_u64(body, 0)?;
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
        Ok(Self {
            previous: Epoch::new(previous),
            records,
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
}

impl<'a> Serving<'a> {
    /// Serve `log` to the peers `log`'s own catalog subscribed.
    #[must_use]
    pub fn declared(log: &'a Store) -> Self {
        Self { log, granted: log }
    }

    /// Serve `log`, asking `granted` who may have it.
    ///
    /// The seam a test uses, and the reason it is here rather than in a test
    /// module: a test about whether a batch applies in order should not have to
    /// declare a peer to find out.
    #[must_use]
    pub fn asking(log: &'a Store, granted: &'a dyn Subscriptions) -> Self {
        Self { log, granted }
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
        let previous = preceding(self.log, over, asked.from)?;
        // A `u64` from a peer against a `usize` here: on a platform where the
        // two differ the ask is larger than anything this node could answer, so
        // the whole log is the honest ceiling.
        let limit = usize::try_from(asked.limit).unwrap_or(usize::MAX);
        let records = self
            .log
            .log_records_within(over, asked.from, limit)
            .map_err(refused)?;
        // What the follower now holds: the last position it was handed, or —
        // when it was handed nothing — the one it told us it was at. The same
        // rule the leader's own door uses, because it is the same event.
        let reached = records.last().map_or_else(
            || Sequence::new(asked.from.get().saturating_sub(1)),
            |(sequence, _)| *sequence,
        );
        self.log.follower_served(follower, reached);
        Ok(Collected { previous, records })
    }
}

/// The leadership that wrote the record before `from`.
///
/// # Errors
///
/// Returns [`Error::Uncollectable`] when this node holds nothing at `from - 1`.
fn preceding(store: &Store, over: Reach, from: Sequence) -> Result<Epoch> {
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
        .log_records_within(over, before, 1)
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
    /// `from` is the first position this node does not hold — ordinarily its own
    /// committed tail plus one. It is a parameter rather than something read
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
    pub fn collect(&self, into: &Store, from: Sequence) -> Result<Sequence> {
        let held = Sequence::new(from.get().saturating_sub(1));
        let (_, answered) = call(
            self.peer.1,
            self.mine.duplicate(),
            self.authority,
            self.peer.0,
            self.said,
            Ask::Records(Collect {
                from,
                limit: self.limit,
            }),
        )?;
        let Answered::Collected(collected) = answered else {
            return Err(Error::OutOfTurn {
                tag: crate::peer::PeerFrame::Collected.tag(),
            });
        };

        let carried = u64::try_from(collected.records.len()).unwrap_or(u64::MAX);
        let mut previous = collected.previous;
        let mut reached = held;
        for (at, record) in &collected.records {
            into.apply_from_stream(*at, previous, record)
                .map_err(refused)?;
            previous = record.epoch();
            reached = *at;
        }

        // Short means the peer had no more, which is the one moment this node
        // can observe that its copy was current. A full answer is contact and
        // not arrival, and recording it as arrival would admit exactly the read
        // a staleness bound exists to exclude.
        let currency = if carried < self.limit {
            Currency::Level
        } else {
            Currency::Behind
        };
        into.collected(reached, currency);
        Ok(reached)
    }
}

/// The store's own words, carried through rather than reworded.
fn refused(why: tessari_storage::Error) -> Error {
    Error::Refused {
        message: why.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Collect, Collected, Collector, Reach, Result, Serving};
    use crate::error::Error;
    use crate::grant::Deciding;
    use crate::link::tests::{Authority, THERE, hello, settled};
    use crate::link::{Answered, Ask, Peers, call};
    use crate::peer::Purpose;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tessari_encoding::{LogRecord, NODE_ID_LEN};
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
        for (index, epoch) in epochs.iter().enumerate() {
            let at = Sequence::new(
                u64::try_from(index)
                    .expect("a handful of records")
                    .saturating_add(1),
            );
            db.store()
                .apply_record(at, &LogRecord::at(Epoch::new(*epoch), Vec::new()))
                .expect("an empty record applies at the next position");
        }
        db
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
            drop(peers.greet(
                &mine,
                &Deciding::holding(settled()),
                &Serving::declared(db.store()),
            ));
        });
        (address, door)
    }

    /// A peer door for `LEADER` that serves `rounds` connections out of `db`.
    fn serving(authority: &Authority, db: &Arc<Db>, rounds: usize) -> (SocketAddr, JoinHandle<()>) {
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
                    &mine,
                    &Deciding::holding(settled()),
                    &Serving::asking(db.store(), &Everything),
                ));
            }
        });
        (address, door)
    }

    /// Ask the door at `address` for the records after `from`.
    fn collect(
        authority: &Authority,
        address: SocketAddr,
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
            from: Sequence::new(7),
            limit: 64,
        };
        assert_eq!(Collect::decode(&asked.encode()).expect("an ask"), asked);

        let answer = Collected {
            previous: Epoch::new(3),
            records: vec![
                (Sequence::new(7), LogRecord::at(Epoch::new(4), Vec::new())),
                (Sequence::new(8), LogRecord::at(Epoch::new(4), Vec::new())),
            ],
        };
        let back = Collected::decode(&answer.encode()).expect("an answer");
        assert_eq!(back, answer);
        // The leadership before the batch travels separately from the ones
        // inside it, and they differ here on purpose: a codec that carried one
        // of them twice would pass a test where they were equal.
        assert_eq!(back.previous, Epoch::new(3));
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
    fn a_subscriber_receives_its_namespace_and_not_the_one_beside_it() {
        let authority = Authority::new();
        let leader = granting(" REPLICATES NAMESPACE prod");
        let (address, door) = declaring(&authority, &leader);

        let follower = Db::in_memory().expect("an in-memory store");
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let reached = collector(&mine, &der, &said, address, 1024)
            .collect(follower.store(), Sequence::new(1))
            .expect("a subscribed peer collects");
        assert!(reached.get() > 1, "the leader had a log to hand over");
        door.join().expect("the door's thread");

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
            .collect(follower.store(), Sequence::new(1))
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
            .collect(follower.store(), Sequence::new(1))
            .expect("a collection applies");
        door.join().expect("the door's thread");

        assert_eq!(reached, Sequence::new(3));
        assert_eq!(
            follower
                .store()
                .log_records(Sequence::new(1), 64)
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
            .collect(follower.store(), Sequence::new(1))
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

        let follower = logged(&[1, 1]);
        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let refused =
            collector(&mine, &der, &said, address, 64).collect(follower.store(), Sequence::new(3));
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
                .log_records(Sequence::new(1), 64)
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
            .collect(follower.store(), Sequence::new(1))
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
            .collect(follower.store(), Sequence::new(3))
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
