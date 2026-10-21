//! Metadata: this node as the one broker, and the topics asked about.

use std::net::SocketAddr;
use std::sync::Arc;

use tessaridb::Parameters;

use crate::api::metadata::{Broker, Topic};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::running::Held;
use crate::surface::{listing, names};

/// The id this node answers as, broker and controller alike.
pub(crate) const NODE_ID: i32 = 1;

/// The broker Metadata names: the advertised address, else the one reached.
pub(crate) fn broker(shared: &Shared, reached: Option<SocketAddr>) -> Broker {
    let (host, port) = match (&shared.advertised, reached) {
        (Some((host, port)), _) => (host.clone(), *port),
        (None, Some(address)) => (address.ip().to_string(), i32::from(address.port())),
        (None, None) => (
            "localhost".to_owned(),
            i32::from(tessari_constants::KAFKA_DEFAULT_PORT),
        ),
    };
    Broker {
        node_id: NODE_ID,
        host,
        port,
    }
}

/// One topic's description.
async fn describe(shared: &Arc<Shared>, held: &mut Held, name: &str) -> Topic {
    let Some(topic) = names::topic(name, shared.database.as_ref()) else {
        return Topic {
            error_code: errors::UNKNOWN_TOPIC_OR_PARTITION,
            name: name.to_owned(),
            partitions: 0,
        };
    };
    let script = format!("INFO FOR TOPIC {};", topic.name);
    match held
        .run(shared, topic.tenancy(), &script, &Parameters::new())
        .await
    {
        Ok(_) => Topic {
            error_code: errors::NONE,
            name: name.to_owned(),
            partitions: 1,
        },
        Err(failed) => Topic {
            error_code: code(&failed),
            name: name.to_owned(),
            partitions: 0,
        },
    }
}

/// The topics asked about, or — asked for all — every topic the caller may
/// see, across every database it may read.
pub(crate) async fn topics(
    shared: &Arc<Shared>,
    held: &mut Held,
    asked: Option<&[&str]>,
) -> Vec<Topic> {
    if let Some(asked) = asked {
        let mut topics = Vec::with_capacity(asked.len());
        for name in asked {
            topics.push(describe(shared, held, name).await);
        }
        return topics;
    }
    listing::visible(shared, held)
        .await
        .into_iter()
        .map(|name| Topic {
            error_code: errors::NONE,
            name,
            partitions: 1,
        })
        .collect()
}
