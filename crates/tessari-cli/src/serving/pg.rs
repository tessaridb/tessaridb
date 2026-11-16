//! The wire-protocol surface, bound and configured from the flags and the
//! environment: off unless named, like the RESP and Kafka surfaces, so a node
//! starts with exactly the surfaces the operator asked for.

use crate::arguments::Serving;

/// The address the surface binds when `--pg` is absent; empty is off.
const PG: &str = "TESSARIDB_PG";

/// The namespace a connection lands in when its startup names none; empty
/// leaves the node's own resolution — the surface's built-in default.
const PG_NAMESPACE: &str = "TESSARIDB_PG_NAMESPACE";

fn flag_or_variable(flag: Option<&String>, variable: &str) -> Option<String> {
    flag.cloned()
        .or_else(|| std::env::var(variable).ok())
        .filter(|written| !written.is_empty() && written != "off")
}

/// The surface, when one was asked for — bound before anything serves, so an
/// address that cannot be taken stops the start rather than a surface quietly
/// going missing.
pub(super) fn bind(
    db: &std::sync::Arc<tessaridb::Db>,
    serving: &Serving,
    secured: Option<&tessari_serve::tls::Credential>,
) -> Result<Option<tessari_pg::Node>, String> {
    let Some(address) = flag_or_variable(serving.pg.as_ref(), PG) else {
        return Ok(None);
    };
    let mut node = tessari_pg::Node::bind(std::sync::Arc::clone(db), address.as_str())
        .map_err(|failure| format!("{address}: {failure}"))?;
    if let Some(namespace) = flag_or_variable(serving.pg_namespace.as_ref(), PG_NAMESPACE) {
        node = node.defaulting_namespace(namespace);
    }
    if let Some(credential) = secured {
        node = node.securing(credential.server_config(&[]));
    }
    if secured.is_none() {
        tracing::warn!(
            address = %address,
            "the wire-protocol surface serves in the clear: no certificate, no TLS"
        );
    }
    Ok(Some(node))
}

/// Count the surface in the census and give the stages its stop, answering the
/// token its listener stops on.
pub(super) fn count(
    node: Option<&tessari_pg::Node>,
    census: &mut tessari_serve::Census,
    surfaces: &mut Vec<crate::shutdown::Surface>,
) -> tokio_util::sync::CancellationToken {
    let stops = tokio_util::sync::CancellationToken::new();
    if let Some(node) = node {
        census.counting("pg", node.stopping());
        let stop = stops.clone();
        surfaces.push(crate::shutdown::Surface {
            name: "pg",
            stopping: node.stopping(),
            wake: Box::new(move || stop.cancel()),
        });
    }
    stops
}

/// Serve it until its token is cancelled.
pub(super) fn listen(
    listening: &mut tokio::task::JoinSet<()>,
    node: Option<tessari_pg::Node>,
    stop: tokio_util::sync::CancellationToken,
) {
    if let Some(node) = node {
        listening.spawn(crate::supervise::listener("pg", async move {
            node.serve(stop).await
        }));
    }
}
