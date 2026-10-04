use super::*;

/// Say what was bound, on the error stream: each client surface, whether its
/// clients are encrypted, and the peer door when there is one.
pub(super) fn announce(
    wire: Option<&tessari_wire::Node>,
    http: Option<&tessari_http::Node>,
    clients: &crate::tls::Clients,
    peers: Option<&Peering>,
) -> Result<(), String> {
    let mut client_addresses = Vec::with_capacity(2);
    if let Some(node) = &wire {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        tracing::info!(address = %bound, "serving the wire protocol");
        client_addresses.push(bound);
    }
    if let Some(node) = &http {
        let bound = node.address();
        tracing::info!(address = %bound, "serving http");
        client_addresses.push(bound);
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
            let reach = crate::tls::reach(&client_addresses);
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
    Ok(())
}
