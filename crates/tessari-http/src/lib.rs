//! An HTTP surface for TessariDB.
//!
//! A route that runs a script, and three that say how the node is: whether it is
//! alive, whether it will take work, and the numbers behind both. Not a REST
//! resource tree over tables — that would be a second query
//! language, expressed in URLs, that can say less than the one this store
//! already has. **The language is the API.**
//!
//! # Synchronous, deliberately
//!
//! A listener and a thread per request, with no async runtime. That matches
//! every layer below: a commit is a compare-and-set against a substrate, and it
//! is synchronous by design. A runtime here would not stay here — the facade
//! would grow async constructors, the session async methods, and the store would
//! end up behind `spawn_blocking` at every call — which is a large,
//! hard-to-reverse decision to take as a side effect of wanting an endpoint.
//!
//! The cost is stated: a thread per concurrent request. That is right for an
//! embedded store and a single node, and wrong for ten thousand idle
//! connections. When that is the problem it belongs to the wire protocol, which
//! will take the async decision deliberately because it will need to.
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

mod basic;
#[cfg(feature = "console")]
mod console;
/// Without the `console` feature the binary carries none of the console's bytes,
/// so every path it would have served falls through to the ordinary 404 — which
/// is exactly how a build that does not want a console is meant to answer.
#[cfg(not(feature = "console"))]
mod console {
    use tiny_http::Method;

    use crate::respond::Answer;

    pub(crate) const fn asset(_method: &Method, _path: &str) -> Option<Answer> {
        None
    }
}
mod json;
mod object;
mod request;
mod respond;
mod tokens;
mod websocket;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tessari_constants::MAX_CONNECTIONS;
use tessari_serve::{Admitting, Busy, Census, Stopping};
use tessaridb::Db;
use tessaridb::feed::Commits;
use tiny_http::{Method, Request, Response, Server};

pub use respond::Answer;

/// A node listening for HTTP requests.
pub struct Node {
    db: Arc<Db>,
    server: Arc<Server>,
    stopping: Arc<Stopping>,
    census: Option<Arc<Census>>,
    /// The wake-up subscribers on this node wait on.
    ///
    /// Per node rather than per store: a commit that arrived through a different
    /// surface does not signal this one, and that costs a subscriber up to one
    /// wait rather than costing it the change (`tessaridb::feed::Commits`).
    committed: Arc<Commits>,
    door: Arc<Admitting>,
    /// The sessions this node has handed out.
    ///
    /// Per node, so a token issued at one surface does not open another, and in
    /// memory, so a restart invalidates every one of them (`tokens`).
    tokens: Arc<tokens::Tokens>,
}

/// What ends a node's accept loop from another thread.
///
/// Separate from [`Stopping`] because the two answer different questions:
/// `Stopping` is the shared *intent* every surface reads, and this is the one
/// mechanical act only this surface can perform. It also keeps `tiny_http` out
/// of the caller's vocabulary — a caller holding the server directly would be
/// holding this crate's dependency.
pub struct Halt {
    server: Arc<Server>,
}

impl Halt {
    /// Wake the accept loop so it can see that stopping was asked for.
    ///
    /// Ordering matters and belongs to the caller: set the intent **first**,
    /// then call this, or the loop can find nothing set and block again.
    pub fn wake(&self) {
        self.server.unblock();
    }
}

impl Node {
    /// Listen on `address`, serving `db`.
    ///
    /// # Errors
    ///
    /// Returns an error when the address cannot be bound.
    pub fn bind(
        db: Arc<Db>,
        address: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            db,
            server: Arc::new(Server::http(address)?),
            stopping: Stopping::new(),
            census: None,
            committed: Arc::new(Commits::default()),
            door: Admitting::to(MAX_CONNECTIONS),
            tokens: Arc::new(tokens::Tokens::default()),
        })
    }

    /// Report on every surface in `census`, not only on this one.
    ///
    /// Set after binding rather than taken by [`Node::bind`], because the census
    /// names surfaces and one of them is this node — a process cannot hand over
    /// a list it can only finish building once every listener exists. Nothing
    /// observes the gap: the node is not serving yet.
    pub fn watching(&mut self, census: Arc<Census>) {
        self.census = Some(census);
    }

    /// The address actually bound, which is what a caller needs when it asked
    /// for port zero.
    #[must_use]
    pub fn address(&self) -> String {
        self.server
            .server_addr()
            .to_ip()
            .map_or_else(|| "unknown".to_owned(), |found| found.to_string())
    }

    /// What this node counts as in flight, and how it is told to stop.
    ///
    /// Taken **before** [`Node::serve`], which borrows the node for as long as
    /// it runs.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// The handle that ends this node's accept loop.
    #[must_use]
    pub fn halt(&self) -> Halt {
        Halt {
            server: Arc::clone(&self.server),
        }
    }

    /// Serve requests until the process ends.
    ///
    /// Each request is handled on its own thread and in its own session: no
    /// cookies, no connection state, no `USE` that outlives a request. A script
    /// says what it operates on, and session state across requests is
    /// authentication's problem rather than something to invent half of here.
    pub fn serve(&self) {
        for request in self.server.incoming_requests() {
            if self.stopping.asked() {
                break;
            }
            let db = Arc::clone(&self.db);
            let stopping = Arc::clone(&self.stopping);
            let census = self.census.clone();
            let committed = Arc::clone(&self.committed);
            let tokens = Arc::clone(&self.tokens);
            // Counted before the thread starts, not inside it: a shutdown that
            // began between the accept and the spawn would otherwise drain to
            // zero while this request had not started.
            let mut busy = self.stopping.busy();
            let id = next_request();
            // Before the thread, for the reason the wire node gives: the thread
            // is the resource. A refused request is answered — 503 with a
            // `Retry-After`, which is what a load balancer acts on — and that
            // answer costs no thread.
            let Some(place) = self.door.admit() else {
                log::warn!(
                    "request {id} refused: {} already in flight",
                    self.door.limit()
                );
                stopping.answered(true);
                drop(
                    request.respond(
                        Response::from_string(
                            r#"{"error":"this node is answering as many requests as it will"}"#,
                        )
                        .with_status_code(503)
                        .with_header(
                            "Retry-After: 1"
                                .parse::<tiny_http::Header>()
                                .unwrap_or_else(|()| {
                                    tiny_http::Header::from_bytes(&b"Retry-After"[..], &b"1"[..])
                                        .unwrap_or_else(|()| {
                                            unreachable!("a constant header parses")
                                        })
                                }),
                        ),
                    ),
                );
                continue;
            };
            log::info!(
                "request {id} {} {} from {}",
                request.method(),
                request.url(),
                request.remote_addr().map_or_else(
                    || "an address tiny_http would not give".to_owned(),
                    std::string::ToString::to_string
                )
            );
            // A panic in one request must not take the listener with it, and a
            // thread is what gives that for free.
            std::thread::spawn(move || {
                // Released on drop, panic included.
                let _place = place;
                answer(
                    id,
                    &Serving {
                        db: &db,
                        tokens: &tokens,
                        stopping: &stopping,
                        census: census.as_deref(),
                        committed: &committed,
                    },
                    &mut busy,
                    request,
                );
            });
        }
    }

    /// Handle exactly one request, for a caller driving the loop itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener fails.
    pub fn serve_one(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = self.server.recv()?;
        let mut busy = self.stopping.busy();
        let id = next_request();
        answer(
            id,
            &Serving {
                db: &self.db,
                tokens: &self.tokens,
                stopping: &self.stopping,
                census: self.census.as_deref(),
                committed: &self.committed,
            },
            &mut busy,
            request,
        );
        Ok(())
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

/// What every request on this node shares.
///
/// Gathered into one borrow rather than passed as five, because they are one
/// thing — the node — and threading them individually is how a sixth gets added
/// to some call sites and not others.
struct Serving<'a> {
    db: &'a Db,
    tokens: &'a tokens::Tokens,
    stopping: &'a Stopping,
    census: Option<&'a Census>,
    committed: &'a Commits,
}

/// Route one request and write its answer.
fn answer(id: u64, node: &Serving<'_>, busy: &mut Busy, mut request: Request) {
    let Serving {
        db,
        tokens,
        stopping,
        census,
        committed,
    } = *node;
    let route = (request.method().clone(), request.url().to_owned());
    // Taken before the shared reply path because an upgrade consumes the
    // request: the socket outlives this function and there is no `Answer` to
    // hand back. Counted here for the same reason — a route that returns early
    // past the one place every answer is counted is a route the scrape silently
    // forgets.
    if route.0 == Method::Get && route.1 == "/watch" {
        let refused = websocket::watch(db, stopping, committed, tokens, request, busy);
        stopping.answered(refused);
        return;
    }
    // Read before the body, because `as_reader` borrows the request mutably and
    // the headers are wanted either way.
    let presented = basic::presented(
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Authorization"))
            .map(|header| header.value.as_str()),
    );
    let reply = match (&route.0, route.1.as_str()) {
        // Health carries no data, so it answers a listening socket the same way
        // for everyone: a load balancer must not need a credential to tell a
        // live node from a dead one.
        (Method::Get, "/health") => respond::health(db),
        // A different question, and a supervisor acts on the two in opposite
        // ways — a readiness failure means stop sending traffic, a liveness
        // failure means restart. No credential, for the same reason health
        // needs none.
        (Method::Get, "/ready") => respond::ready(db, stopping.ready()),
        // Also without a credential, and for the third time the same reason: a
        // scraper that needs one is a scraper nobody configures. What it carries
        // is operational — an uptime, a sequence, some counts — with no user
        // data and no schema in it.
        (Method::Get, "/metrics") => respond::metrics(db, census, stopping, tokens),
        // Split on `?` here rather than reaching for a URL parser: this route
        // takes one optional parameter and a dependency to read it would be a
        // poor trade.
        (Method::Get, url) if url == "/backup" || url.starts_with("/backup?") => respond::backup(
            db,
            url.split_once('?').map(|(_, query)| query),
            tokens,
            &presented,
        ),
        // Where a password is spent, once, for a token that stands in for it
        // afterwards. Both halves are here rather than only the first: a
        // credential a client cannot hand back is one it holds until it exits.
        (Method::Post, "/session") => respond::open_session(db, &presented, tokens),
        (Method::Delete, "/session") => respond::close_session(&presented, tokens),
        (Method::Post, "/script") => {
            // The body's shape is decided by what the caller says it is, not by
            // sniffing a leading brace: HTTP has a field for this, and a rule
            // nobody can look up is a rule nobody can rely on. A plain body is
            // the script, which is what it has always been.
            let json = request.headers().iter().any(|header| {
                header.field.equiv("Content-Type")
                    && header
                        .value
                        .as_str()
                        .to_ascii_lowercase()
                        .contains("application/json")
            });
            let mut body = String::new();
            match request.as_reader().read_to_string(&mut body) {
                Err(_) => Answer::bad_request("the request body is not text"),
                Ok(_) if !json => {
                    respond::script(db, &body, &Default::default(), tokens, &presented)
                }
                Ok(_) => match request::envelope(&body) {
                    Ok(read) => {
                        respond::script(db, &read.script, &read.parameters, tokens, &presented)
                    }
                    Err(reason) => Answer::bad_request(&reason),
                },
            }
        }
        // "No such thing" and "not that way" are different answers, and a caller
        // debugging a client needs to know which one it got.
        (_, "/script" | "/session" | "/health" | "/ready" | "/metrics" | "/watch") => Answer::new(
            405,
            r#"{"error":"that route takes another method"}"#.to_owned(),
        ),
        (method, url) => match object::target(url) {
            Some(aimed) => match method {
                Method::Put | Method::Post => {
                    let mut body = Vec::new();
                    match request.as_reader().read_to_end(&mut body) {
                        Ok(_) => object::put(db, &aimed, body, tokens, &presented),
                        Err(_) => Answer::bad_request("the request body could not be read"),
                    }
                }
                Method::Get | Method::Head => object::get(db, &aimed, tokens, &presented),
                Method::Delete => object::delete(db, &aimed, tokens, &presented),
                _ => Answer::new(
                    405,
                    r#"{"error":"that route takes another method"}"#.to_owned(),
                ),
            },
            // Last, and deliberately so: the console never shadows a route, it
            // only fills paths nothing else claimed. With the feature off there
            // is nothing to fill them with and this is the ordinary 404.
            None => console::asset(method, url)
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

    // A statement that succeeded may have committed something, so wake this
    // node's subscribers rather than leaving them to notice on their next
    // timeout. Not required for correctness — `Commits::wait` returns anyway —
    // and the signal is deliberately coarse: it says "look", never what changed,
    // so it cannot disagree with the log about what actually happened.
    if route.0 == Method::Post && route.1 == "/script" && reply.status < 400 {
        committed.signal();
    }

    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    // A `401` without a challenge is not a `401` a client can act on — RFC 9110
    // requires the header, so it follows from the status rather than from a
    // separate decision at each place that produces one.
    if reply.status == 401
        && let Ok(header) =
            r#"WWW-Authenticate: Basic realm="TessariDB""#.parse::<tiny_http::Header>()
    {
        response = response.with_header(header);
    }
    // The answer says what it is. A constant either way, so it parses — and if it
    // somehow did not, an answer without a content type still beats no answer.
    if let Ok(header) = format!("Content-Type: {}", reply.kind).parse::<tiny_http::Header>() {
        response = response.with_header(header);
    }
    // A client that hung up mid-answer is not this node's problem, and there is
    // nobody left to tell.
    drop(request.respond(response));
}
