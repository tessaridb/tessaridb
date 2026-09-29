//! An HTTP surface for TessariDB.
//!
//! A route that runs a script, and three that say how the node is: whether it is
//! alive, whether it will take work, and the numbers behind both. Not a REST
//! resource tree over tables — that would be a second query
//! language, expressed in URLs, that can say less than the one this store
//! already has. **The language is the API.**
//!
//! # On the runtime, with the store behind a bridge
//!
//! Requests are served by axum on the process's runtime (ADR-0085): a waiting
//! connection costs a task, not a thread. The routes themselves are unchanged
//! and synchronous — a commit is a compare-and-set against a substrate — so each
//! request's route runs on the blocking pool through the node's bridge, which
//! refuses rather than queues when every slot is taken.
//!
//! # How a request says who it is
//!
//! `Authorization: Basic`, and nothing else. A store with no users declared is
//! **open** and runs anything, which is what keeps an empty one usable; the
//! first `DEFINE USER` closes it, and from then on a request without a
//! credential is answered `401`.
//!
//! Basic over plaintext is plaintext: the password is in a header anything on
//! the path can read. This is a credential for a connection an operator already
//! protects — a loopback bind, or a reverse proxy terminating TLS — and the
//! README says so rather than leaving it to be discovered.

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod basic;
mod body;
#[cfg(feature = "console")]
mod console;
#[cfg(not(feature = "console"))]
mod console {
    use axum::http::Method;

    use crate::respond::Answer;

    pub(crate) const fn asset(_method: &Method, _path: &str) -> Option<Answer> {
        None
    }
}
mod incoming;
mod json;
mod kv;
mod listening;
mod object;
mod request;
mod respond;
mod tokens;
mod websocket;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::serve::ListenerExt;
use tessari_constants::{MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{Admitting, Bridge, Bridged, Census, Stopping};
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio_util::sync::CancellationToken;

use crate::incoming::Incoming;
pub use respond::Answer;

/// One wire session over a byte stream: given the stream, it runs until the
/// session ends (ADR-0089).
pub type WireSession = Box<
    dyn FnOnce(tokio::io::DuplexStream) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>
        + Send,
>;

/// The wire node's door, as `GET /wire` needs it: a place for one session, or
/// `None` when the wire node is serving as many connections as it will.
///
/// Handed in by the process rather than built here, because this crate does not
/// depend on the wire protocol's crate and a WebSocket is only ever the carrier.
pub type WireDoor = Arc<dyn Fn() -> Option<WireSession> + Send + Sync>;

/// An HTTP listener bound to one address, serving one store.
pub struct Node {
    db: Arc<Db>,
    listener: TcpListener,
    stopping: Arc<Stopping>,
    census: Option<Arc<Census>>,
    committed: Arc<Commits>,
    door: Arc<Admitting>,
    tokens: Arc<tokens::Tokens>,
    /// How many requests' routes may run on the blocking pool at once.
    bridge: Arc<Bridge>,
    /// How many watch rounds may run at once, apart from requests — the
    /// wire node's reason, and measured there: rounds bunch on one commit.
    rounds: Arc<Bridge>,
    /// The wire node's door, when this process serves the wire protocol too.
    wire: Option<WireDoor>,
}

impl Node {
    /// Bind `address`.
    ///
    /// Bound here, synchronously, so a process learns whether it has its
    /// address before it builds anything else; served by [`Node::serve`] on the
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(
        db: Arc<Db>,
        address: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(address)?;
        // The runtime's listener requires it, and nothing here reads it blocking.
        listener.set_nonblocking(true)?;
        let committed = Arc::clone(db.commits());
        Ok(Self {
            db,
            listener,
            stopping: Stopping::new(),
            census: None,
            committed,
            door: Admitting::to(MAX_CONNECTIONS),
            tokens: Arc::new(tokens::Tokens::default()),
            bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
            rounds: Arc::new(Bridge::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            wire: None,
        })
    }

    /// Carry the wire protocol over `GET /wire`, through `door` (ADR-0089).
    ///
    /// Without it the route answers `404`: a node started without a wire
    /// address has no wire sessions to hand out, over any carrier.
    pub fn carrying(&mut self, door: WireDoor) {
        self.wire = Some(door);
    }

    /// Report every surface of this process on `/metrics`, not only this one.
    ///
    /// Set after binding rather than taken by [`Node::bind`], because the census
    /// names this node among the others and so is only complete once it exists.
    pub fn watching(&mut self, census: Arc<Census>) {
        self.census = Some(census);
    }

    /// The address actually bound, which a caller needs when it asked for port zero.
    #[must_use]
    pub fn address(&self) -> String {
        self.listener
            .local_addr()
            .map_or_else(|_| "unknown".to_owned(), |found| found.to_string())
    }

    /// What this node counts as in flight, and how it is told to stop.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Serve until `stop` is cancelled.
    ///
    /// Must be awaited inside a Tokio runtime; the node creates none of its own.
    /// Cancelling stops accepting and lets requests in flight finish; a watch
    /// ends at the stage that ends feeds, when [`Stopping`] says so.
    ///
    /// # Errors
    ///
    /// Returns the listener's failure.
    pub async fn serve(&self, stop: CancellationToken) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let shared = Arc::new(Shared {
            db: Arc::clone(&self.db),
            tokens: Arc::clone(&self.tokens),
            stopping: Arc::clone(&self.stopping),
            census: self.census.clone(),
            committed: Arc::clone(&self.committed),
            door: Arc::clone(&self.door),
            bridge: Arc::clone(&self.bridge),
            rounds: Arc::clone(&self.rounds),
            wire: self.wire.clone(),
        });
        let app = axum::Router::new().fallback(handle).with_state(shared);
        let mut listening = listening::Listening::new(listener);
        let (ended, failure) = (listening.ended(), listening.failure());
        axum::serve(
            // Tapped for the address alone: axum hands a peer's address to
            // `ConnectInfo` only through its own listener or a tapped one.
            listening.tap_io(|_| {}),
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = stop.cancelled() => {}
                () = ended.cancelled() => {}
            }
        })
        .await?;
        failure
            .and_then(|mut failed| failed.try_recv().ok())
            .map_or(Ok(()), Err)
    }
}

/// Names one request across every line it produces.
///
/// A request rather than a connection, because this surface holds no state
/// between them: no cookies, no session, no `USE` that outlives one. Following a
/// client across requests is authentication's job, and this number does not
/// pretend to do it.
static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// The next request's name.
fn next_request() -> u64 {
    REQUESTS.fetch_add(1, Ordering::Relaxed)
}

/// What every request on this node shares, owned so a task can hold it.
pub(crate) struct Shared {
    pub(crate) db: Arc<Db>,
    pub(crate) tokens: Arc<tokens::Tokens>,
    pub(crate) stopping: Arc<Stopping>,
    census: Option<Arc<Census>>,
    pub(crate) committed: Arc<Commits>,
    door: Arc<Admitting>,
    bridge: Arc<Bridge>,
    pub(crate) rounds: Arc<Bridge>,
    pub(crate) wire: Option<WireDoor>,
}

/// Admit one request, read its body if its route takes one, and answer it.
async fn handle(
    State(node): State<Arc<Shared>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
) -> Response {
    // Counted before anything else, not after: a shutdown that began between
    // the accept and here would otherwise drain to zero while this request had
    // not started.
    let busy = node.stopping.busy();
    let id = next_request();
    // Before the route, for the reason the wire node gives: the route's slot is
    // the resource. A refused request is answered — 503 with a `Retry-After`,
    // which is what a load balancer acts on.
    let Some(place) = node.door.admit() else {
        log::warn!(
            "request {id} refused: {} already in flight",
            node.door.limit()
        );
        node.stopping.answered(true);
        return refused_at_the_door();
    };
    let (mut parts, body) = request.into_parts();
    let url = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);
    log::info!("request {id} {} {url} from {from}", parts.method);
    // Taken before the shared reply path because an upgrade consumes the
    // request: the socket outlives this function. Counted inside, for the same
    // reason every other answer is counted once.
    if parts.method == Method::GET && url == "/watch" {
        return websocket::watch(node, &mut parts, busy, place).await;
    }
    if parts.method == Method::GET && url == "/wire" {
        return websocket::wire(node, &mut parts, busy, place).await;
    }
    let read = if incoming::takes_body(&parts.method, &url) {
        Some(incoming::read(&parts.headers, body).await)
    } else {
        None
    };
    let incoming = Incoming {
        method: parts.method,
        url,
        headers: parts.headers,
        body: read,
    };
    let routing = Arc::clone(&node);
    let bridged = node
        .bridge
        .call(incoming, move |incoming| answer(id, &routing, incoming))
        .await;
    drop(place);
    drop(busy);
    match bridged {
        Bridged::Answered(reply) => to_response(reply),
        Bridged::Busy(_) => {
            node.stopping.answered(true);
            refused_at_the_door()
        }
        // A panic in one request takes that request down and nothing else: the
        // listener goes on answering everybody after it.
        Bridged::Panicked => {
            log::error!("request {id} panicked");
            node.stopping.answered(true);
            to_response(Answer::new(
                500,
                r#"{"error":"the request failed inside the node"}"#.to_owned(),
            ))
        }
    }
}

/// The admission refusal, as it has always read.
fn refused_at_the_door() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"this node is answering as many requests as it will"}"#,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    response
}

/// Route one request and say what to answer.
///
/// Runs on the blocking pool: every route here is synchronous.
fn answer(id: u64, node: &Shared, mut request: Incoming) -> Answer {
    let db = node.db.as_ref();
    let tokens = node.tokens.as_ref();
    let stopping = node.stopping.as_ref();
    let census = node.census.as_deref();
    let route = (request.method.clone(), request.url.clone());
    let presented = basic::presented(request.header("Authorization"));
    let reply = match (route.0.clone(), route.1.as_str()) {
        // Health carries no data, so it answers a listening socket the same way
        // for everyone: a load balancer must not need a credential to tell a
        // live node from a dead one.
        (Method::GET, "/health") => respond::health(db),
        // A different question, and a supervisor acts on the two in opposite
        // ways — a readiness failure means stop sending traffic, a liveness
        // failure means restart. No credential, for the same reason health
        // needs none.
        (Method::GET, "/ready") => respond::ready(db, stopping.ready()),
        // Also without a credential, and for the third time the same reason: a
        // scraper that needs one is a scraper nobody configures. What it carries
        // is operational — an uptime, a sequence, some counts — with no user
        // data and no schema in it. A scraper that does present one is also
        // given each topic it may read, by name (G042).
        (Method::GET, "/metrics") => respond::metrics(db, census, stopping, tokens, &presented),
        // Split on `?` here rather than reaching for a URL parser: this route
        // takes one optional parameter and a dependency to read it would be a
        // poor trade.
        (Method::GET, url) if url == "/backup" || url.starts_with("/backup?") => respond::backup(
            db,
            url.split_once('?').map(|(_, query)| query),
            tokens,
            &presented,
        ),
        // Where a password is spent, once, for a token that stands in for it
        // afterwards. Both halves are here rather than only the first: a
        // credential a client cannot hand back is one it holds until it exits.
        (Method::POST, "/session") => respond::open_session(db, &presented, tokens),
        (Method::DELETE, "/session") => respond::close_session(&presented, tokens),
        // Basic only, deliberately: the second proof is the whole route, and a
        // token is not proof of a password.
        (Method::POST, "/password") => match body::text(&mut request) {
            Ok(body) => respond::change_password(db, &presented, body.trim_end_matches('\n')),
            Err(refused) => refused,
        },
        (Method::POST, "/script") => {
            // The body's shape is decided by what the caller says it is, not by
            // sniffing a leading brace: HTTP has a field for this, and a rule
            // nobody can look up is a rule nobody can rely on. A plain body is
            // the script, which is what it has always been.
            let json = request
                .header("Content-Type")
                .is_some_and(|kind| kind.to_ascii_lowercase().contains("application/json"));
            match body::text(&mut request) {
                Err(refused) => refused,
                Ok(body) if !json => {
                    respond::script(db, &body, &Default::default(), tokens, &presented)
                }
                Ok(body) => match request::envelope(&body) {
                    Ok(read) => {
                        respond::script(db, &read.script, &read.parameters, tokens, &presented)
                    }
                    Err(reason) => Answer::bad_request(&reason),
                },
            }
        }
        // A batch of events for one series, in one transaction (G044 C12).
        (Method::POST, url) if url.starts_with("/series/") => match respond::series::target(url) {
            Some(aimed) => match body::text(&mut request) {
                Ok(body) => respond::series::append(db, &aimed, &body, tokens, &presented),
                Err(refused) => refused,
            },
            None => Answer::new(404, r#"{"error":"no such route"}"#.to_owned()),
        },
        (method, url) if url.starts_with("/kv/") => match kv::target(url) {
            Some(aimed) => match body::text(&mut request) {
                Ok(text) => kv::answer(db, &method, &aimed, &text, tokens, &presented),
                Err(refused) => refused,
            },
            None => Answer::new(404, r#"{"error":"no such route"}"#.to_owned()),
        },
        (_, url) if url.starts_with("/series/") => Answer::new(
            405,
            r#"{"error":"that route takes another method"}"#.to_owned(),
        ),
        // "No such thing" and "not that way" are different answers, and a caller
        // debugging a client needs to know which one it got.
        (
            _,
            "/script" | "/session" | "/password" | "/health" | "/ready" | "/metrics" | "/watch"
            | "/wire",
        ) => Answer::new(
            405,
            r#"{"error":"that route takes another method"}"#.to_owned(),
        ),
        (method, url) => match object::target(url) {
            Some(aimed) => match method {
                Method::PUT | Method::POST => match body::bytes(&mut request) {
                    Ok(body) => object::put(db, &aimed, body, tokens, &presented),
                    Err(refused) => refused,
                },
                Method::GET | Method::HEAD => object::get(db, &aimed, tokens, &presented),
                Method::DELETE => object::delete(db, &aimed, tokens, &presented),
                _ => Answer::new(
                    405,
                    r#"{"error":"that route takes another method"}"#.to_owned(),
                ),
            },
            // Last, and deliberately so: the console never shadows a route, it
            // only fills paths nothing else claimed. With the feature off there
            // is nothing to fill them with and this is the ordinary 404.
            None => console::asset(&method, url)
                .unwrap_or_else(|| Answer::new(404, r#"{"error":"no such route"}"#.to_owned())),
        },
    };

    // Counted here because this is the one place every answer this surface
    // writes passes through, which is what keeps "what a refusal is" a single
    // decision rather than one taken again at each route.
    stopping.answered(reply.status >= 400);

    // Reported at the same single place, and at a level the status decides: a
    // 404 is traffic and a 500 is an event, and an operator filtering by level
    // should not have to know which routes produce which.
    if reply.status >= 500 {
        log::error!("request {id} answered {}", reply.status);
    } else if reply.status >= 400 {
        log::warn!("request {id} answered {}", reply.status);
    } else {
        log::info!("request {id} answered {}", reply.status);
    }

    reply
}

/// An answer as axum sends it, with the headers its status obliges.
fn to_response(reply: Answer) -> Response {
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, reply.body).into_response();
    let headers = response.headers_mut();
    // A `401` without a challenge is not a `401` a client can act on — RFC 9110
    // requires the header, so it follows from the status rather than from a
    // separate decision at each place that produces one.
    if reply.status == 401 {
        headers.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Basic realm="TessariDB""#),
        );
    }
    // A `307` without a `Location` is not a redirect a client can act on — the
    // `401` rule applied to the other status that carries an obligation, except
    // that the address follows from the answer rather than from the status.
    if let Some(where_to) = &reply.location
        && let Ok(value) = HeaderValue::from_str(where_to)
    {
        headers.insert(header::LOCATION, value);
    }
    // The answer says what it is; an answer without a content type still beats
    // no answer if a kind ever failed to be a header.
    if let Ok(value) = HeaderValue::from_str(reply.kind) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    response
}
