//! A connection that listens (ADR-0133 D1): its subscriptions, the store's
//! word on whether its user may listen or publish, and the messages waiting
//! for it.
//!
//! # Who may listen, who may publish
//!
//! A channel is not a thing the store's grants can name, so the space the
//! connection reads stands for it: listening needs `read` on that space and
//! publishing needs `write`. Neither is decided here. A statement asks the
//! store — a read of a key for the first, a write inside a transaction that is
//! cancelled for the second, which the store refuses or allows exactly as it
//! would the real thing and lands nothing. The answer is kept for
//! `RESP_PUBSUB_RECHECK_SECONDS`; a connection that keeps listening is asked
//! again on that cadence, and one refused then is closed, its subscriptions
//! with it.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use super::Connection;
use crate::codec::{Command, Protocol, Reply};
use crate::surface::Space;
use crate::surface::channels::{Channels, Delivery, Inbox, Kind, Scope};
use crate::surface::commands::{Asked, Listening, listening_command};
use crate::surface::replies;
use crate::surface::running::Failed;
use crate::surface::statement::Binder;

/// The key the read probe asks for: one no command names, since a RESP key is
/// bound as text or bytes and this one begins with a byte no text does.
const PROBE_KEY: &[u8] = b"\xfftessaridb.pubsub";

/// A connection's subscriptions, while it has any.
pub(super) struct Subscribed {
    channels: Arc<Channels>,
    scope: Scope,
    id: u64,
    inbox: Arc<Inbox>,
    pub(super) receiver: mpsc::Receiver<Delivery>,
    plain: Vec<Vec<u8>>,
    shards: Vec<Vec<u8>>,
    patterns: Vec<Vec<u8>>,
    /// When the store last said this user may read the space.
    pub(super) checked: Instant,
}

impl Subscribed {
    fn names(&mut self, what: Listening) -> &mut Vec<Vec<u8>> {
        match what {
            Listening::Channels => &mut self.plain,
            Listening::Shards => &mut self.shards,
            Listening::Patterns => &mut self.patterns,
        }
    }

    /// The count a notice carries: channels and patterns together, or shard
    /// channels alone, as the protocol counts them.
    fn count(&self, what: Listening) -> i64 {
        let held = match what {
            Listening::Shards => self.shards.len(),
            Listening::Channels | Listening::Patterns => {
                self.plain.len().saturating_add(self.patterns.len())
            }
        };
        i64::try_from(held).unwrap_or(i64::MAX)
    }

    /// Everything it listens on, of every kind.
    fn held(&self) -> usize {
        self.plain
            .len()
            .saturating_add(self.shards.len())
            .saturating_add(self.patterns.len())
    }

    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.shards.is_empty() && self.patterns.is_empty()
    }

    fn join(&mut self, what: Listening, name: &[u8]) {
        if self.names(what).iter().any(|held| held == name) {
            return;
        }
        match what {
            Listening::Channels => {
                self.channels
                    .listen(&self.scope, Kind::Plain, name, self.id, &self.inbox);
            }
            Listening::Shards => {
                self.channels
                    .listen(&self.scope, Kind::Shard, name, self.id, &self.inbox);
            }
            Listening::Patterns => {
                self.channels
                    .listen_pattern(&self.scope, name, self.id, &self.inbox);
            }
        }
        self.names(what).push(name.to_vec());
    }

    fn part(&mut self, what: Listening, name: &[u8]) -> bool {
        let names = self.names(what);
        let Some(at) = names.iter().position(|held| held == name) else {
            return false;
        };
        names.remove(at);
        match what {
            Listening::Channels => self.channels.leave(&self.scope, Kind::Plain, name, self.id),
            Listening::Shards => self.channels.leave(&self.scope, Kind::Shard, name, self.id),
            Listening::Patterns => self.channels.leave_pattern(&self.scope, name, self.id),
        }
        true
    }

    /// Whether its inbox overflowed and it must be cut off.
    pub(super) fn is_cut(&self) -> bool {
        self.inbox.is_cut()
    }

    /// `delivery` was taken off the inbox.
    pub(super) fn taken(&self, delivery: &Delivery) {
        self.inbox.taken(delivery);
    }
}

impl Drop for Subscribed {
    fn drop(&mut self) {
        for name in std::mem::take(&mut self.plain) {
            self.channels
                .leave(&self.scope, Kind::Plain, &name, self.id);
        }
        for name in std::mem::take(&mut self.shards) {
            self.channels
                .leave(&self.scope, Kind::Shard, &name, self.id);
        }
        for pattern in std::mem::take(&mut self.patterns) {
            self.channels.leave_pattern(&self.scope, &pattern, self.id);
        }
    }
}

fn scope_of(space: &Space) -> Scope {
    Scope {
        namespace: space.namespace.clone(),
        database: space.database.clone(),
    }
}

/// What a probe the store refused answers.
fn refused(failed: &Failed) -> Reply {
    replies::failed(failed)
}

impl Connection {
    /// Whether the connection listens on anything.
    pub(super) fn is_listening(&self) -> bool {
        self.subscribed.is_some()
    }

    /// The reply to `command` when a listening RESP2 connection may not run
    /// it, or the protocol answers it differently there.
    pub(super) fn while_listening(&self, command: &Command) -> Option<Reply> {
        if !self.is_listening() || self.protocol != Protocol::Two {
            return None;
        }
        let name = command.first()?.to_ascii_uppercase();
        let name = String::from_utf8_lossy(&name);
        if name == "PING" {
            let said = command.get(1).cloned().unwrap_or_default();
            return Some(Reply::Array(vec![Reply::text("pong"), Reply::Bulk(said)]));
        }
        if listening_command(&name) {
            return None;
        }
        Some(Reply::error(format!(
            "ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / \
             RESET are allowed in this context",
            name.to_ascii_lowercase()
        )))
    }

    /// Ask the store whether this user may read the connection's space.
    async fn may_read(&mut self, space: &Space) -> Result<(), Reply> {
        let mut binder = Binder::default();
        let probe = format!("GET {}", binder.key(space, PROBE_KEY));
        self.held
            .run(&self.shared, space, &probe, binder.into_parameters())
            .await
            .map(drop)
            .map_err(|failed| refused(&failed))
    }

    /// Ask the store whether this user may write the connection's space,
    /// writing nothing.
    async fn may_write(&mut self, space: &Space) -> Result<(), Reply> {
        if self
            .published
            .is_some_and(|checked| checked.elapsed() < self.shared.recheck)
        {
            return Ok(());
        }
        let mut binder = Binder::default();
        let target = binder.key(space, PROBE_KEY);
        let value = binder.bind(tessaridb::Value::from(1_i64));
        let probe = format!("BEGIN; SET {target} = {value}; CANCEL;");
        self.held
            .run(&self.shared, space, &probe, binder.into_parameters())
            .await
            .map_err(|failed| refused(&failed))?;
        self.published = Some(Instant::now());
        Ok(())
    }

    /// `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`: one notice per name.
    pub(super) async fn listen(&mut self, what: Listening, names: Vec<Vec<u8>>) -> Reply {
        let Some(space) = self.space.clone() else {
            return replies::no_space();
        };
        if let Err(reply) = self.may_read(&space).await {
            return reply;
        }
        let held = self.subscribed.as_ref().map_or(0, Subscribed::held);
        if held.saturating_add(names.len()) > self.shared.subscriptions {
            return Reply::error(format!(
                "ERR a connection listens on at most {} channels and patterns",
                self.shared.subscriptions
            ));
        }
        let subscribed = self.subscribed.get_or_insert_with(|| {
            let (messages, bytes) = self.shared.inbox;
            let (inbox, receiver) = Inbox::new(messages, bytes);
            Subscribed {
                channels: Arc::clone(&self.shared.channels),
                scope: scope_of(&space),
                id: self.id,
                inbox,
                receiver,
                plain: Vec::new(),
                shards: Vec::new(),
                patterns: Vec::new(),
                checked: Instant::now(),
            }
        });
        subscribed.checked = Instant::now();
        let mut notices = Vec::with_capacity(names.len());
        for name in names {
            subscribed.join(what, &name);
            notices.push(Reply::Push(vec![
                Reply::text(what.word(true)),
                Reply::Bulk(name),
                Reply::Integer(subscribed.count(what)),
            ]));
        }
        Reply::Several(notices)
    }

    /// `UNSUBSCRIBE`, `PUNSUBSCRIBE`, `SUNSUBSCRIBE`: one notice per name
    /// left, every one held when none is named, and one with no name when
    /// nothing was held.
    pub(super) fn leave(&mut self, what: Listening, names: Vec<Vec<u8>>) -> Reply {
        let word = what.word(false);
        let Some(subscribed) = self.subscribed.as_mut() else {
            return Reply::Push(vec![Reply::text(word), Reply::Nil, Reply::Integer(0)]);
        };
        let names = if names.is_empty() {
            subscribed.names(what).clone()
        } else {
            names
        };
        let notices: Vec<Reply> = if names.is_empty() {
            vec![Reply::Push(vec![
                Reply::text(word),
                Reply::Nil,
                Reply::Integer(subscribed.count(what)),
            ])]
        } else {
            names
                .into_iter()
                .map(|name| {
                    subscribed.part(what, &name);
                    Reply::Push(vec![
                        Reply::text(word),
                        Reply::Bulk(name),
                        Reply::Integer(subscribed.count(what)),
                    ])
                })
                .collect()
        };
        if subscribed.is_empty() {
            self.subscribed = None;
        }
        Reply::Several(notices)
    }

    /// `PUBLISH`, `SPUBLISH`: how many subscriptions on this node it reached.
    pub(super) async fn publish(&mut self, kind: Kind, channel: &[u8], message: &[u8]) -> Reply {
        let Some(space) = self.space.clone() else {
            return replies::no_space();
        };
        if let Err(reply) = self.may_write(&space).await {
            return reply;
        }
        Reply::Integer(
            self.shared
                .channels
                .publish(&scope_of(&space), kind, channel, message),
        )
    }

    /// `PUBSUB …`: what this node's channels in the connection's database
    /// hold.
    pub(super) async fn pubsub(&mut self, asked: Asked) -> Reply {
        let Some(space) = self.space.clone() else {
            return replies::no_space();
        };
        if let Err(reply) = self.may_read(&space).await {
            return reply;
        }
        let scope = scope_of(&space);
        let channels = &self.shared.channels;
        match asked {
            Asked::Channels(kind, pattern) => Reply::Array(
                channels
                    .channels(&scope, kind, pattern.as_deref())
                    .into_iter()
                    .map(Reply::Bulk)
                    .collect(),
            ),
            // A flat array of channel and count in both versions, as the
            // protocol answers it: clients read it by pairs of items.
            Asked::Listeners(kind, names) => Reply::Array(
                names
                    .into_iter()
                    .flat_map(|name| {
                        let count = channels.listeners(&scope, kind, &name);
                        [Reply::Bulk(name), Reply::Integer(count)]
                    })
                    .collect(),
            ),
            Asked::Patterns => Reply::Integer(channels.patterns(&scope)),
        }
    }

    /// Ask the store again whether this listening connection's user may
    /// still read its space, when the last answer is older than the cadence;
    /// `false` when it may not, and the connection is to close.
    pub(super) async fn still_listening(&mut self) -> bool {
        let due = self
            .subscribed
            .as_ref()
            .is_some_and(|subscribed| subscribed.checked.elapsed() >= self.shared.recheck);
        if !due {
            return true;
        }
        let Some(space) = self.space.clone() else {
            return false;
        };
        match self.may_read(&space).await {
            Ok(()) => {
                if let Some(subscribed) = self.subscribed.as_mut() {
                    subscribed.checked = Instant::now();
                }
                true
            }
            // A busy node is asked again on the next round.
            Err(Reply::Error(text)) if text.starts_with("TRYAGAIN") => true,
            Err(_) => {
                tracing::info!("resp subscriber closed: its user may no longer read the space");
                false
            }
        }
    }

    /// Forget what the store said about this user, as a new sign-in must.
    pub(super) fn forget_authority(&mut self) {
        self.published = None;
        if let Some(subscribed) = self.subscribed.as_mut() {
            subscribed.checked = Instant::now()
                .checked_sub(self.shared.recheck)
                .unwrap_or_else(Instant::now);
        }
    }
}
