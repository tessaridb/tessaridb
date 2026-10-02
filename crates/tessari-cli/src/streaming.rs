//! Following a leader on a held stream instead of on a clock (ADR-0106 D5).
//!
//! # What runs where
//!
//! The collection round still runs every interval and is still the only thing
//! that joins, re-seeds, or meets a refusal. Once a round against a leader has
//! succeeded, it starts ONE stream to that leader, and while the stream is alive
//! the round leaves that leader alone. The stream applies what the leader sends
//! through the round's own apply (`Collector::apply`), so there is one apply
//! path; it ends on any refusal, on silence, on the leader no longer being this
//! line's upstream, or on stop — and the next round takes over from the
//! positions the store actually holds.
//!
//! # Why a thread and not a task — the recorded reason
//!
//! A stream blocks on the leader's next frame for as long as nothing is
//! committed, and the peer link is synchronous by design (`link.rs`). A runtime
//! worker must never be the thing that blocks, and the blocking pool is for work
//! that ends, so each stream owns one named thread for its lifetime. There is
//! one per leader followed — on a store line, one.
//!
//! # The positions come from the store, not from a cursor
//!
//! Each ask names the first position this node does not hold in each log, read
//! from the store's committed tails after the previous round applied. A cursor
//! kept beside the store could disagree with it after a failure; the tails
//! cannot, and they are exactly what a write waiting for copies needs the leader
//! to be told (ADR-0106 D1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tessari_constants::{COLLECTION_RECORDS, STREAM_HEARTBEAT_MILLIS};
use tessari_storage::{Currency, NODE_ID_LEN, Reach, Writer};
use tessari_types::Sequence;
use tessaridb::Db;
use tokio_util::sync::CancellationToken;

use crate::greeting_round::{greeting, hex};

/// How long a leader may say nothing before its stream counts as lost: ten
/// heartbeats.
const SILENCE: Duration = Duration::from_millis(STREAM_HEARTBEAT_MILLIS * 10);

/// Which logs a stream carries, asked again before every ask, because a
/// namespace arrives through the stream itself.
pub(crate) type HomesFor = dyn Fn(&Db) -> Option<Vec<Reach>> + Send;

/// [`HomesFor`], owned by the stream's thread.
pub(crate) type Homes = Box<HomesFor>;

/// Whether the leader is still the one this line follows, asked before every
/// ask; `false` ends the stream and the round decides again.
pub(crate) type StillFor = dyn Fn(&Db) -> bool + Send;

/// [`StillFor`], owned by the stream's thread.
pub(crate) type Still = Box<StillFor>;

/// A stream's key: the leader, and the line it leads for this node — `None` for
/// the store line, the range for a placed one. One leader can lead both.
pub(crate) type Line = ([u8; NODE_ID_LEN], Option<Reach>);

/// One running stream: its liveness, published by the thread as it ends.
#[derive(Debug)]
struct Running {
    alive: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// The streams this node holds, one per leader it follows.
///
/// Owned by the one collection loop that starts them, so a plain map: nothing
/// else reaches it, and the threads publish only their own liveness flag.
#[derive(Debug, Default)]
pub(crate) struct Streams {
    running: std::collections::BTreeMap<Line, Running>,
}

impl Streams {
    /// Whether a stream for `line` is running right now. Ended streams are
    /// joined here, which costs nothing: their flag is published last.
    pub(crate) fn following(&mut self, line: Line) -> bool {
        let ended: Vec<Line> = self
            .running
            .iter()
            .filter(|(_, running)| !running.alive.load(Ordering::Acquire))
            .map(|(line, _)| *line)
            .collect();
        for ended in ended {
            if let Some(running) = self.running.remove(&ended)
                && running.thread.join().is_err()
            {
                log::warn!("the stream from {} ended in a panic", hex(&ended.0));
            }
        }
        self.running.contains_key(&line)
    }

    /// Start a stream to `leader` at `address` — unless one is already running.
    pub(crate) fn start(
        &mut self,
        db: Arc<Db>,
        (mine, authority): (
            tessari_wire::Credential,
            tessari_wire::CertificateDer<'static>,
        ),
        (line, address): (Line, std::net::SocketAddr),
        (homes, still): (Homes, Still),
        (stop, wakes): (CancellationToken, Arc<crate::peers::Wakes>),
    ) {
        if self.following(line) {
            return;
        }
        let leader = line.0;
        let alive = Arc::new(AtomicBool::new(true));
        let published = Arc::clone(&alive);
        let started = std::thread::Builder::new()
            .name(format!("stream-{}", hex(&leader)))
            .spawn(move || {
                let ended = follow(
                    &db,
                    (mine, &authority),
                    (leader, address),
                    (&*homes, &*still),
                    &stop,
                );
                match ended {
                    Ok(()) => log::info!(
                        "the stream from {} ended; the collection round takes over",
                        hex(&leader)
                    ),
                    Err(why) => log::warn!(
                        "the stream from {} ended: {why}; the collection round takes over",
                        hex(&leader)
                    ),
                }
                // Last, and Release: whoever reads `false` may join at once.
                published.store(false, Ordering::Release);
                // A stream that ended is the first sign its leader may be gone,
                // so the greeting round learns where the line went now rather
                // than at its next interval (G053 SG2b).
                wakes.greeting.notify_one();
            });
        match started {
            Ok(thread) => {
                log::info!("following {} on a held stream", hex(&leader));
                self.running.insert(line, Running { alive, thread });
            }
            Err(why) => log::warn!("a stream thread could not be started: {why}"),
        }
    }

    /// Wait for every stream to end. Called off the runtime once `stop` is
    /// cancelled; each stream notices within one heartbeat, or within
    /// [`SILENCE`] when its leader has gone quiet.
    pub(crate) fn join(self) {
        for ((leader, _), running) in self.running {
            if running.thread.join().is_err() {
                log::warn!("the stream from {} ended in a panic", hex(&leader));
            }
        }
    }
}

/// Hold one stream until it ends, applying every round the leader sends.
fn follow(
    db: &Db,
    (mine, authority): (
        tessari_wire::Credential,
        &tessari_wire::CertificateDer<'static>,
    ),
    (leader, address): ([u8; NODE_ID_LEN], std::net::SocketAddr),
    (homes, still): (&HomesFor, &StillFor),
    stop: &CancellationToken,
) -> Result<(), String> {
    let store = db.store();
    let said = greeting(db).map_err(|why| why.to_string())?;
    let mut following = tessari_wire::Following::open(
        (leader, address),
        mine.duplicate(),
        authority,
        &said,
        SILENCE,
    )
    .map_err(|why| why.to_string())?;
    let collector = tessari_wire::Collector {
        mine: &mine,
        authority,
        said: &said,
        peer: (leader, address),
        limit: COLLECTION_RECORDS,
    };
    loop {
        if stop.is_cancelled() || !still(db) {
            return Ok(());
        }
        let Some(logs) = homes(db) else {
            return Err("this node cannot say which logs it should hold".to_owned());
        };
        let mut asks = Vec::with_capacity(logs.len());
        for home in logs {
            // The log the leader serves for this home (ADR-0107).
            let log = store
                .followed_log(home, Writer::new(leader))
                .map_err(|why| why.to_string())?;
            let tail = store.committed_tail(log).map_err(|why| why.to_string())?;
            asks.push((home, Sequence::new(tail.get().saturating_add(1))));
        }
        let asked = tessari_wire::StreamAsk {
            asks: asks
                .iter()
                .map(|(home, from)| tessari_wire::Collect {
                    home: *home,
                    from: *from,
                    limit: COLLECTION_RECORDS,
                })
                .collect(),
        };
        following.ask(&asked).map_err(|why| why.to_string())?;
        let round = loop {
            let frame = following.heard().map_err(|why| why.to_string())?;
            if !frame.is_heartbeat() {
                break frame;
            }
            // Nothing after these positions has landed on the leader, so this
            // copy is level as of now — the reading a staleness bound asks for.
            if let Some((_, from)) = asks.first() {
                store.collected(Sequence::new(from.get().saturating_sub(1)), Currency::Level);
            }
            // Asked on every beat and not only before an ask: a leader demoted
            // while idle goes on beating for a log nobody writes any more, and a
            // follower that believed it would miss its successor's writes.
            if stop.is_cancelled() || !still(db) {
                return Ok(());
            }
        };
        if round.answers.len() != asks.len() {
            return Err(format!(
                "the leader answered {} logs to an ask for {}",
                round.answers.len(),
                asks.len()
            ));
        }
        let applied = collector.apply(store, &asks, round.answers.into_iter().map(Ok).collect());
        for ((home, _), result) in asks.iter().zip(applied) {
            if let Err(why) = result {
                return Err(format!("applying {home:?} was refused: {why}"));
            }
        }
    }
}
