//! Who listens to what on this node's RESP surface (ADR-0133 D1).
//!
//! Pub/sub keeps nothing: a message published goes to the subscribers present
//! at that moment and is gone. So the whole state is this registry — every
//! channel and pattern with the connections listening on it — and a message is
//! handed to each one's inbox as it is published.
//!
//! # A channel belongs to one database
//!
//! The protocol's channels ignore the numbered databases, but a store's tenancy
//! cannot be ignored: two applications in two databases of one node must not
//! hear each other. A channel is therefore named within the database of the
//! space its connection reads.
//!
//! # A full inbox cuts its subscriber off
//!
//! Publishing never waits for a subscriber. One that does not read fills its
//! inbox, and is cut off: the message is not given to it, and its connection
//! closes as soon as it next looks. A slow subscriber costs this node a bounded
//! amount of memory and never a silent gap — the client sees a disconnect.
//!
//! # Locks
//!
//! Each map's guard is held only for the synchronous work of one call and never
//! across an `.await`; publishing is a `try_send` under a shard's read lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::codec::{Protocol, encode_push};
use crate::surface::glob;

/// The database a channel is named in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Scope {
    pub(crate) namespace: String,
    pub(crate) database: String,
}

/// Plain channels and shard channels are two names spaces, as the protocol has
/// them: a shard message reaches only shard subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Plain,
    Shard,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Channel {
    scope: Scope,
    kind: Kind,
    name: Vec<u8>,
}

/// One published message on its way to one subscriber; its bytes are shared by
/// every subscriber it went to.
#[derive(Debug, Clone)]
pub(crate) struct Delivery {
    kind: Kind,
    pattern: Option<Arc<[u8]>>,
    channel: Arc<[u8]>,
    payload: Arc<[u8]>,
}

impl Delivery {
    /// What it holds this subscriber's place for, in bytes.
    fn size(&self) -> usize {
        self.channel
            .len()
            .saturating_add(self.payload.len())
            .saturating_add(self.pattern.as_ref().map_or(0, |pattern| pattern.len()))
    }

    /// Write it as the protocol's message notice.
    pub(crate) fn encode(&self, protocol: Protocol, out: &mut Vec<u8>) {
        match (&self.pattern, self.kind) {
            (Some(pattern), _) => encode_push(
                &[b"pmessage", pattern, &self.channel, &self.payload],
                protocol,
                out,
            ),
            (None, Kind::Plain) => {
                encode_push(&[b"message", &self.channel, &self.payload], protocol, out);
            }
            (None, Kind::Shard) => {
                encode_push(&[b"smessage", &self.channel, &self.payload], protocol, out);
            }
        }
    }
}

/// One connection's waiting messages, as the publishers see it.
#[derive(Debug)]
pub(crate) struct Inbox {
    sender: mpsc::Sender<Delivery>,
    queued: AtomicUsize,
    byte_limit: usize,
    cut: AtomicBool,
}

impl Inbox {
    /// An inbox holding at most `messages` deliveries and `bytes` of them.
    pub(crate) fn new(messages: usize, bytes: usize) -> (Arc<Self>, mpsc::Receiver<Delivery>) {
        let (sender, receiver) = mpsc::channel(messages.max(1));
        (
            Arc::new(Self {
                sender,
                queued: AtomicUsize::new(0),
                byte_limit: bytes,
                cut: AtomicBool::new(false),
            }),
            receiver,
        )
    }

    /// Hand `delivery` over, answering whether it was; one that does not fit
    /// cuts the subscriber off for good.
    fn offer(&self, delivery: Delivery) -> bool {
        if self.cut.load(Ordering::Acquire) {
            return false;
        }
        let size = delivery.size();
        let before = self.queued.fetch_add(size, Ordering::AcqRel);
        if before.saturating_add(size) > self.byte_limit || self.sender.try_send(delivery).is_err()
        {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            self.cut.store(true, Ordering::Release);
            return false;
        }
        true
    }

    /// `delivery` was taken off the inbox by its connection.
    pub(crate) fn taken(&self, delivery: &Delivery) {
        self.queued.fetch_sub(delivery.size(), Ordering::AcqRel);
    }

    /// Whether this subscriber was cut off for not reading.
    pub(crate) fn is_cut(&self) -> bool {
        self.cut.load(Ordering::Acquire)
    }
}

/// The connections listening on one channel, by connection.
type Listening = DashMap<u64, Arc<Inbox>>;

/// The patterns listened on in one database, by connection and pattern.
type Patterned = DashMap<(u64, Vec<u8>), Arc<Inbox>>;

/// Every subscription on this node.
#[derive(Debug, Default)]
pub(crate) struct Channels {
    listeners: DashMap<Channel, Listening>,
    patterns: DashMap<Scope, Patterned>,
}

impl Channels {
    /// Connection `id` listens on `name`.
    pub(crate) fn listen(
        &self,
        scope: &Scope,
        kind: Kind,
        name: &[u8],
        id: u64,
        inbox: &Arc<Inbox>,
    ) {
        self.listeners
            .entry(Channel {
                scope: scope.clone(),
                kind,
                name: name.to_vec(),
            })
            .or_default()
            .insert(id, Arc::clone(inbox));
    }

    /// Connection `id` stops listening on `name`.
    pub(crate) fn leave(&self, scope: &Scope, kind: Kind, name: &[u8], id: u64) {
        let channel = Channel {
            scope: scope.clone(),
            kind,
            name: name.to_vec(),
        };
        if let Some(listening) = self.listeners.get(&channel) {
            listening.remove(&id);
        }
        self.listeners
            .remove_if(&channel, |_, listening| listening.is_empty());
    }

    /// Connection `id` listens on every channel `pattern` matches.
    pub(crate) fn listen_pattern(
        &self,
        scope: &Scope,
        pattern: &[u8],
        id: u64,
        inbox: &Arc<Inbox>,
    ) {
        self.patterns
            .entry(scope.clone())
            .or_default()
            .insert((id, pattern.to_vec()), Arc::clone(inbox));
    }

    /// Connection `id` stops listening on `pattern`.
    pub(crate) fn leave_pattern(&self, scope: &Scope, pattern: &[u8], id: u64) {
        if let Some(listening) = self.patterns.get(scope) {
            listening.remove(&(id, pattern.to_vec()));
        }
        self.patterns
            .remove_if(scope, |_, listening| listening.is_empty());
    }

    /// Publish `payload` on `name`, answering how many subscriptions it
    /// reached — one connection listening on the channel and on a pattern
    /// matching it is reached twice, as the protocol counts.
    pub(crate) fn publish(&self, scope: &Scope, kind: Kind, name: &[u8], payload: &[u8]) -> i64 {
        let channel: Arc<[u8]> = Arc::from(name);
        let payload: Arc<[u8]> = Arc::from(payload);
        let mut reached = 0_i64;
        let key = Channel {
            scope: scope.clone(),
            kind,
            name: name.to_vec(),
        };
        if let Some(listening) = self.listeners.get(&key) {
            for inbox in listening.iter() {
                let delivery = Delivery {
                    kind,
                    pattern: None,
                    channel: Arc::clone(&channel),
                    payload: Arc::clone(&payload),
                };
                if inbox.value().offer(delivery) {
                    reached = reached.saturating_add(1);
                }
            }
        }
        if kind == Kind::Plain
            && let Some(listening) = self.patterns.get(scope)
        {
            for entry in listening.iter() {
                let (_, pattern) = entry.key();
                if !glob::matches(pattern, name) {
                    continue;
                }
                let delivery = Delivery {
                    kind,
                    pattern: Some(Arc::from(pattern.as_slice())),
                    channel: Arc::clone(&channel),
                    payload: Arc::clone(&payload),
                };
                if entry.value().offer(delivery) {
                    reached = reached.saturating_add(1);
                }
            }
        }
        reached
    }

    /// The channels of `kind` somebody listens on, those `pattern` matches
    /// when one is given.
    pub(crate) fn channels(
        &self,
        scope: &Scope,
        kind: Kind,
        pattern: Option<&[u8]>,
    ) -> Vec<Vec<u8>> {
        self.listeners
            .iter()
            .filter(|entry| {
                let channel = entry.key();
                channel.scope == *scope
                    && channel.kind == kind
                    && !entry.value().is_empty()
                    && pattern.is_none_or(|pattern| glob::matches(pattern, &channel.name))
            })
            .map(|entry| entry.key().name.clone())
            .collect()
    }

    /// How many connections listen on `name`.
    pub(crate) fn listeners(&self, scope: &Scope, kind: Kind, name: &[u8]) -> i64 {
        let key = Channel {
            scope: scope.clone(),
            kind,
            name: name.to_vec(),
        };
        self.listeners.get(&key).map_or(0, |listening| {
            i64::try_from(listening.len()).unwrap_or(i64::MAX)
        })
    }

    /// How many distinct patterns somebody listens on.
    pub(crate) fn patterns(&self, scope: &Scope) -> i64 {
        self.patterns.get(scope).map_or(0, |listening| {
            let mut distinct: Vec<Vec<u8>> = listening
                .iter()
                .map(|entry| entry.key().1.clone())
                .collect();
            distinct.sort_unstable();
            distinct.dedup();
            i64::try_from(distinct.len()).unwrap_or(i64::MAX)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(database: &str) -> Scope {
        Scope {
            namespace: "app".to_owned(),
            database: database.to_owned(),
        }
    }

    #[test]
    fn a_subscriber_that_does_not_read_is_cut_off_and_given_nothing_more() {
        let channels = Channels::default();
        let (inbox, mut receiver) = Inbox::new(2, 1_024);
        channels.listen(&scope("main"), Kind::Plain, b"news", 1, &inbox);
        for _ in 0..2 {
            assert_eq!(
                channels.publish(&scope("main"), Kind::Plain, b"news", b"x"),
                1
            );
        }
        assert!(!inbox.is_cut());
        // The third finds the inbox full: not counted, and the subscriber is
        // cut off even once there is room again.
        assert_eq!(
            channels.publish(&scope("main"), Kind::Plain, b"news", b"x"),
            0
        );
        assert!(inbox.is_cut());
        let taken = receiver.try_recv().unwrap();
        inbox.taken(&taken);
        assert_eq!(
            channels.publish(&scope("main"), Kind::Plain, b"news", b"x"),
            0
        );
        // Bytes cut off a subscriber the count would not.
        let (small, _kept) = Inbox::new(100, 10);
        channels.listen(&scope("main"), Kind::Plain, b"big", 2, &small);
        assert_eq!(
            channels.publish(&scope("main"), Kind::Plain, b"big", b"0123456789"),
            0
        );
        assert!(small.is_cut());
    }

    #[test]
    fn a_message_reaches_its_database_kind_and_patterns_only() {
        let channels = Channels::default();
        let (here, mut heard) = Inbox::new(16, 1_024);
        let (there, mut elsewhere) = Inbox::new(16, 1_024);
        channels.listen(&scope("main"), Kind::Plain, b"news", 1, &here);
        channels.listen_pattern(&scope("main"), b"n*", 1, &here);
        channels.listen(&scope("other"), Kind::Plain, b"news", 2, &there);
        channels.listen(&scope("main"), Kind::Shard, b"news", 2, &there);
        assert_eq!(
            channels.publish(&scope("main"), Kind::Plain, b"news", b"x"),
            2
        );
        assert!(heard.try_recv().is_ok());
        assert!(heard.try_recv().is_ok());
        assert!(elsewhere.try_recv().is_err());
        assert_eq!(channels.listeners(&scope("main"), Kind::Plain, b"news"), 1);
        assert_eq!(channels.patterns(&scope("main")), 1);
        channels.leave(&scope("main"), Kind::Plain, b"news", 1);
        channels.leave_pattern(&scope("main"), b"n*", 1);
        assert_eq!(
            channels.publish(&scope("main"), Kind::Plain, b"news", b"x"),
            0
        );
        // Nothing listening leaves nothing behind.
        assert!(
            channels
                .channels(&scope("main"), Kind::Plain, None)
                .is_empty()
        );
        assert_eq!(channels.patterns(&scope("main")), 0);
        assert_eq!(channels.listeners.len(), 2);
        assert!(channels.patterns.is_empty());
    }
}
