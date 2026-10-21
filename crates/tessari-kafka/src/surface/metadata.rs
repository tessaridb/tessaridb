//! Metadata: a broker for this node and every member a Kafka client can
//! reach, and the topics asked about, each led by the broker of the node
//! leading its database (G079 W5d).

use std::net::SocketAddr;
use std::sync::Arc;

use tessaridb::{NODE_ID_LEN, Parameters, Reach};

use crate::api::metadata::{Broker, Topic};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::running::Held;
use crate::surface::{grouping, listing, names};

/// The id a node standing alone answers as, broker and controller alike.
pub(crate) const NODE_ID: i32 = 1;

/// The brokers Metadata names, numbered alike on every node of a cluster.
pub(crate) struct Brokers {
    /// Every member node's id, sorted: a node's broker id is its place here
    /// plus one, so a node standing alone is [`NODE_ID`].
    nodes: Vec<[u8; NODE_ID_LEN]>,
    me: [u8; NODE_ID_LEN],
    /// This node first, then every peer whose row says `KAFKA AT`.
    pub(crate) listed: Vec<Broker>,
}

impl Brokers {
    /// This node's broker.
    pub(crate) fn this(&self) -> &Broker {
        &self.listed[0]
    }

    /// The broker of `node` when Metadata names it, else this node's: a client
    /// sent to a node it cannot reach is answered where it already is.
    fn of(&self, node: &[u8; NODE_ID_LEN]) -> i32 {
        let id = self.id(node);
        if self.listed.iter().any(|broker| broker.node_id == id) {
            id
        } else {
            self.this().node_id
        }
    }

    /// The broker coordinating this surface's groups: the node leading the
    /// database bare names are read in — the store line's leader when none is
    /// named — when Metadata names it, else this node.
    pub(crate) async fn coordinator(&self, shared: &Shared) -> &Broker {
        let led = match &shared.database {
            Some(bare) => shared
                .db
                .database_led_elsewhere(&bare.namespace, &bare.database)
                .await
                .ok()
                .flatten(),
            None => shared
                .db
                .leader_of(Reach::Store)
                .await
                .ok()
                .flatten()
                .map(|leadership| leadership.node)
                .filter(|node| *node != self.me),
        };
        let id = led.map_or(self.this().node_id, |node| self.of(&node));
        self.listed
            .iter()
            .find(|broker| broker.node_id == id)
            .unwrap_or_else(|| self.this())
    }

    fn id(&self, node: &[u8; NODE_ID_LEN]) -> i32 {
        self.nodes
            .iter()
            .position(|each| each == node)
            .and_then(|place| i32::try_from(place).ok())
            .and_then(|place| place.checked_add(1))
            .unwrap_or(NODE_ID)
    }
}

/// `host:port` as a Kafka client dials it.
fn host_and_port(address: &str) -> Option<(String, i32)> {
    let (host, port) = address.rsplit_once(':')?;
    Some((host.to_owned(), i32::from(port.parse::<u16>().ok()?)))
}

/// This node's broker and every peer's with a Kafka address.
///
/// The peers are the cluster's topology, so a caller not signed in to a store
/// that has users is told of this node alone — where it already is.
pub(crate) async fn brokers(
    shared: &Arc<Shared>,
    held: &mut Held,
    reached: Option<SocketAddr>,
) -> Brokers {
    // A catalog that cannot be read answers this node alone, as before a
    // cluster could be named: the client still has a broker to speak to.
    let (me, rows) = shared
        .db
        .membership()
        .await
        .unwrap_or_else(|_| ([0; NODE_ID_LEN], Vec::new()));
    let mut nodes: Vec<_> = rows.iter().filter_map(|row| row.node).chain([me]).collect();
    nodes.sort_unstable();
    nodes.dedup();
    let mut brokers = Brokers {
        nodes,
        me,
        listed: Vec::with_capacity(rows.len().saturating_add(1)),
    };
    let own_row = rows
        .iter()
        .find(|row| row.node == Some(me))
        .and_then(|row| row.kafka.as_deref())
        .and_then(host_and_port);
    let (host, port) = match (&shared.advertised, own_row, reached) {
        (Some((host, port)), _, _) => (host.clone(), *port),
        (None, Some(row), _) => row,
        (None, None, Some(address)) => (address.ip().to_string(), i32::from(address.port())),
        (None, None, None) => (
            "localhost".to_owned(),
            i32::from(tessari_constants::KAFKA_DEFAULT_PORT),
        ),
    };
    let this = Broker {
        node_id: brokers.id(&me),
        host,
        port,
    };
    brokers.listed.push(this);
    if held.user().is_empty() && !grouping::store_is_open(shared, held).await {
        return brokers;
    }
    for row in &rows {
        let (Some(node), Some((host, port))) =
            (row.node, row.kafka.as_deref().and_then(host_and_port))
        else {
            continue;
        };
        if node != brokers.me {
            let node_id = brokers.id(&node);
            brokers.listed.push(Broker {
                node_id,
                host,
                port,
            });
        }
    }
    brokers
}

/// The broker leading `topic`'s partition: the node leading its database.
async fn leader(shared: &Shared, brokers: &Brokers, topic: &names::Topic<'_>) -> i32 {
    match shared
        .db
        .database_led_elsewhere(topic.namespace, topic.database)
        .await
    {
        Ok(Some(node)) => brokers.of(&node),
        _ => brokers.this().node_id,
    }
}

/// One topic's description.
async fn describe(shared: &Arc<Shared>, held: &mut Held, brokers: &Brokers, name: &str) -> Topic {
    let unknown = |error_code| Topic {
        error_code,
        name: name.to_owned(),
        partitions: 0,
        leader: brokers.this().node_id,
    };
    let Some(topic) = names::topic(name, shared.database.as_ref()) else {
        return unknown(errors::UNKNOWN_TOPIC_OR_PARTITION);
    };
    let script = format!("INFO FOR TOPIC {};", topic.name);
    match held
        .run(shared, topic.tenancy(), &script, &Parameters::new())
        .await
    {
        Ok(outcomes) => Topic {
            error_code: errors::NONE,
            name: name.to_owned(),
            partitions: match outcomes.last() {
                Some(tessaridb::Outcome::Value(info)) => {
                    i32::try_from(crate::surface::offsets::partitions_in(info)).unwrap_or(i32::MAX)
                }
                _ => 1,
            },
            leader: leader(shared, brokers, &topic).await,
        },
        Err(failed) => unknown(code(&failed)),
    }
}

/// The topics asked about, or — asked for all — every topic the caller may
/// see, across every database it may read.
pub(crate) async fn topics(
    shared: &Arc<Shared>,
    held: &mut Held,
    brokers: &Brokers,
    asked: Option<&[&str]>,
) -> Vec<Topic> {
    if let Some(asked) = asked {
        let mut topics = Vec::with_capacity(asked.len());
        for name in asked {
            topics.push(describe(shared, held, brokers, name).await);
        }
        return topics;
    }
    // Described one by one, because each says how many partitions it has.
    let visible = listing::visible(shared, held).await;
    let mut topics = Vec::with_capacity(visible.len());
    for name in visible {
        topics.push(describe(shared, held, brokers, &name).await);
    }
    topics
}
