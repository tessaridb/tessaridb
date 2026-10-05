//! What every answer tells a browser, whichever route produced it.
//!
//! The console carries a session token, so a page another site can frame, or
//! whose content a browser may reinterpret, is a page that can be turned against
//! the operator signed into it. The headers are applied once, to every response
//! the router writes — the snapshot stream and the WebSocket upgrades included —
//! because a header set per route is one a new route forgets.
//!
//! The policy is `'self'` because that is all the console needs: it is built
//! from local assets only (ADR-0076), has no inline script or style, and talks
//! to the node that served it over `fetch` and a same-host WebSocket. Images
//! also allow `data:`, because the stylesheet draws its select arrows as inline
//! SVG; a `data:` image cannot run script.

use axum::http::{HeaderValue, header};
use axum::response::Response;

/// The content policy every answer carries.
const POLICY: &str = "default-src 'self'; img-src 'self' data:; object-src 'none'; \
                      base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Two years, the figure the preload list asks for. Sent only over TLS: a node
/// in the clear that told a browser to insist on TLS would lock that browser out
/// of it.
const TRANSPORT: &str = "max-age=63072000";

/// Add the browser-facing headers to `response`.
pub(crate) fn headers(mut response: Response, secured: bool) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(POLICY),
    );
    if secured {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static(TRANSPORT),
        );
    }
    response
}
