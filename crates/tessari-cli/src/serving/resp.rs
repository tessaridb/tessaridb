//! The RESP surface, bound and configured from the flags and the environment
//! (ADR-0129, owner 2026-10-21: its own port, changeable, so a client changes
//! only its address).

use crate::arguments::Serving;

/// The address the RESP surface binds when `--resp` is absent; empty is off.
const RESP: &str = "TESSARIDB_RESP";
/// The space every RESP connection starts in when `--resp-space` is absent.
const RESP_SPACE: &str = "TESSARIDB_RESP_SPACE";

fn flag_or_variable(flag: Option<&String>, variable: &str) -> Option<String> {
    flag.cloned()
        .or_else(|| std::env::var(variable).ok())
        .filter(|written| !written.is_empty())
}

/// The RESP surface, when one was asked for — bound before anything serves,
/// so an address that cannot be taken stops the start rather than a surface
/// quietly going missing.
pub(super) fn bind(
    db: &std::sync::Arc<tessaridb::Db>,
    serving: &Serving,
    secured: Option<&tessari_serve::tls::Credential>,
) -> Result<Option<tessari_resp::Node>, String> {
    let Some(address) = flag_or_variable(serving.resp.as_ref(), RESP) else {
        return Ok(None);
    };
    let mut node = tessari_resp::Node::bind(std::sync::Arc::clone(db), address.as_str())
        .map_err(|failure| format!("{address}: {failure}"))?;
    if let Some(space) = flag_or_variable(serving.resp_space.as_ref(), RESP_SPACE) {
        let parsed = tessari_resp::Space::parse(&space).ok_or_else(|| {
            format!(
                "{RESP_SPACE} or --resp-space wants <namespace>.<database>.<space>, not {space:?}"
            )
        })?;
        node = node.serving_space(parsed);
    }
    if let Some(credential) = secured {
        node = node.securing(credential.server_config(&[]));
    }
    Ok(Some(node))
}

/// Count the surface in the census and give the stages its stop, answering the
/// token its listener stops on.
pub(super) fn count(
    node: Option<&tessari_resp::Node>,
    census: &mut tessari_serve::Census,
    surfaces: &mut Vec<crate::shutdown::Surface>,
) -> tokio_util::sync::CancellationToken {
    let stops = tokio_util::sync::CancellationToken::new();
    if let Some(node) = node {
        census.counting("resp", node.stopping());
        let stop = stops.clone();
        surfaces.push(crate::shutdown::Surface {
            name: "resp",
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
    node: Option<tessari_resp::Node>,
    stop: tokio_util::sync::CancellationToken,
) {
    if let Some(node) = node {
        listening.spawn(crate::supervise::listener("resp", async move {
            node.serve(stop).await
        }));
    }
}
