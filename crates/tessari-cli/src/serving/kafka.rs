//! The Kafka surface, bound and configured from the flags and the environment
//! (ADR-0129, owner 2026-10-21: its own port, changeable, so a client changes
//! only its bootstrap address).

use crate::arguments::Serving;

/// The address the Kafka surface binds when `--kafka` is absent; empty is off.
const KAFKA: &str = "TESSARIDB_KAFKA";
/// The address Kafka Metadata advertises when `--kafka-advertise` is absent.
const KAFKA_ADVERTISE: &str = "TESSARIDB_KAFKA_ADVERTISE";
/// The database bare Kafka topic names are read in when `--kafka-database` is
/// absent.
const KAFKA_DATABASE: &str = "TESSARIDB_KAFKA_DATABASE";

fn flag_or_variable(flag: Option<&String>, variable: &str) -> Option<String> {
    flag.cloned()
        .or_else(|| std::env::var(variable).ok())
        .filter(|written| !written.is_empty())
}

/// The Kafka surface, when one was asked for — bound before anything serves,
/// so an address that cannot be taken stops the start rather than a surface
/// quietly going missing.
pub(super) fn bind(
    db: &std::sync::Arc<tessaridb::Db>,
    serving: &Serving,
    secured: Option<&tessari_serve::tls::Credential>,
) -> Result<Option<tessari_kafka::Node>, String> {
    let Some(address) = flag_or_variable(serving.kafka.as_ref(), KAFKA) else {
        return Ok(None);
    };
    let mut node = tessari_kafka::Node::bind(std::sync::Arc::clone(db), address.as_str())
        .map_err(|failure| format!("{address}: {failure}"))?;
    if let Some(advertised) = flag_or_variable(serving.kafka_advertise.as_ref(), KAFKA_ADVERTISE) {
        let (host, port) = advertised
            .rsplit_once(':')
            .and_then(|(host, port)| Some((host.to_owned(), port.parse::<u16>().ok()?)))
            .filter(|(host, _)| !host.is_empty())
            .ok_or_else(|| {
                format!(
                    "{KAFKA_ADVERTISE} or --kafka-advertise wants host:port, not {advertised:?}"
                )
            })?;
        node = node.advertising(host, port);
    }
    if let Some(database) = flag_or_variable(serving.kafka_database.as_ref(), KAFKA_DATABASE) {
        let parsed = tessari_kafka::Database::parse(&database).ok_or_else(|| {
            format!("{KAFKA_DATABASE} or --kafka-database wants <namespace>.<database>, not {database:?}")
        })?;
        node = node.reading_bare_names_in(parsed);
    }
    if let Some(credential) = secured {
        node = node.securing(credential.server_config(&[]));
    }
    Ok(Some(node))
}

/// Count the surface in the census and give the stages its stop, answering the
/// token its listener stops on.
pub(super) fn count(
    node: Option<&tessari_kafka::Node>,
    census: &mut tessari_serve::Census,
    surfaces: &mut Vec<crate::shutdown::Surface>,
) -> tokio_util::sync::CancellationToken {
    let stops = tokio_util::sync::CancellationToken::new();
    if let Some(node) = node {
        census.counting("kafka", node.stopping());
        let stop = stops.clone();
        surfaces.push(crate::shutdown::Surface {
            name: "kafka",
            stopping: node.stopping(),
            wake: Box::new(move || stop.cancel()),
        });
    }
    stops
}

/// Serve the surface beside the others, ended by the stage that refuses new
/// connections; a listener that fails ends the node, as every listener does.
pub(super) fn listen(
    listening: &mut tokio::task::JoinSet<()>,
    node: Option<tessari_kafka::Node>,
    stop: tokio_util::sync::CancellationToken,
) {
    if let Some(node) = node {
        listening.spawn(crate::supervise::listener("kafka", async move {
            node.serve(stop).await
        }));
    }
}
