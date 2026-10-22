use super::*;

/// Say what was bound, on the error stream: each client surface, whether its
/// clients are encrypted, and the peer door when there is one. Answers the
/// client surfaces as bound, for `INFO FOR NODE`.
pub(super) fn announce(
    wire: Option<&tessari_wire::Node>,
    http: Option<&tessari_http::Node>,
    kafka: Option<&tessari_kafka::Node>,
    resp: Option<&tessari_resp::Node>,
    clients: &crate::tls::Clients,
    peers: Option<&Peering>,
) -> Result<Vec<tessari_session::Listener>, String> {
    let mut listening = Vec::with_capacity(4);
    if let Some(node) = &wire {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        tracing::info!(address = %bound, "serving the wire protocol");
        listening.push(listener("wire", bound));
    }
    if let Some(node) = &http {
        let bound = node.address();
        tracing::info!(address = %bound, "serving http");
        listening.push(listener("http", bound));
    }
    if let Some(node) = &kafka {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        tracing::info!(address = %bound, "serving the kafka protocol");
        listening.push(listener("kafka", bound));
    }
    if let Some(node) = &resp {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        tracing::info!(address = %bound, "serving the resp protocol");
        listening.push(listener("resp", bound));
    }
    match &clients {
        crate::tls::Clients::Tls { cert, required, .. } => {
            tracing::info!(
                presenting = %cert.display(),
                required = *required,
                "clients over TLS only"
            );
        }
        crate::tls::Clients::Plaintext => {
            let addresses: Vec<String> = listening
                .iter()
                .map(|listener| listener.address.clone())
                .collect();
            let reach = crate::tls::reach(&addresses);
            tracing::warn!(
                reach = %reach,
                "clients in the clear: --tls-cert and --tls-key would encrypt them, and \
                 --require-client-tls refuses to start without them"
            );
        }
    }
    // Said only when there is something to say. Every deployment today is a
    // single node, and a line printed on every start is a line operators stop
    // reading. What was *bound* rather than what was asked for, the same as the
    // two lines above, which is what makes `:0` usable here too.
    if let Some(surface) = &peers {
        let bound = surface
            .door
            .address()
            .map_err(|failure| failure.to_string())?;
        let seeds = surface.seeds.len();
        tracing::info!(address = %bound, seeds, "serving peers");
    }
    Ok(listening)
}

const fn listener(surface: &'static str, address: String) -> tessari_session::Listener {
    tessari_session::Listener { surface, address }
}
