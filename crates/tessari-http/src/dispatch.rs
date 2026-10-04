//! One request, from the route it asked for to the response it gets.

use super::*;

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
    pub(super) census: Option<Arc<Census>>,
    pub(crate) committed: Arc<Commits>,
    pub(super) door: Arc<Admitting>,
    pub(super) bridge: Arc<Bridge>,
    pub(crate) rounds: Arc<Bridge>,
    pub(crate) wire: Option<WireDoor>,
}

/// Admit one request, read its body if its route takes one, and answer it.
pub(super) async fn handle(
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
        tracing::warn!(
            request = id,
            in_flight = node.door.limit(),
            "request refused: as many already in flight as this node takes"
        );
        node.stopping.answered(true);
        return refused_at_the_door();
    };
    let (mut parts, body) = request.into_parts();
    let url = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);
    tracing::info!(request = id, method = %parts.method, url = %url, from = %from, "request received");
    // Taken before the shared reply path because an upgrade consumes the
    // request: the socket outlives this function. Counted inside, for the same
    // reason every other answer is counted once.
    if parts.method == Method::GET && url == "/watch" {
        return websocket::watch(node, &mut parts, busy, place).await;
    }
    if parts.method == Method::GET && url == "/wire" {
        return websocket::wire(node, &mut parts, busy, place).await;
    }
    // A snapshot leaves as it is read rather than as one answer (ADR-0094 D6),
    // so it cannot wait for the bridge to hand back a finished body. The other
    // `/backup` forms are answered whole through the bridge below.
    if parts.method == Method::GET && (url == "/backup" || url == "/backup?as=state") {
        return snapshot::backup(node, &parts, busy, place).await;
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
            tracing::error!(request = id, "request panicked");
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
pub(super) fn answer(id: u64, node: &Shared, mut request: Incoming) -> Answer {
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
        // The vault's own surface. The passphrase is the body and nothing else,
        // so it is never script text; the body is trimmed of one trailing line
        // end exactly as `/password`'s is, so `curl -d @file` works.
        (Method::GET, "/vault") => respond::vault::answer(
            db,
            tessaridb::VaultTarget::Store,
            tessaridb::VaultAct::Status,
            tokens,
            &presented,
        ),
        (Method::POST, "/vault/seal") => respond::vault::answer(
            db,
            tessaridb::VaultTarget::Store,
            tessaridb::VaultAct::Seal,
            tokens,
            &presented,
        ),
        (Method::POST, "/vault/passphrase") => match body::text(&mut request) {
            Ok(body) => match request::passphrases(&body) {
                Ok((current, new)) => respond::vault::answer(
                    db,
                    tessaridb::VaultTarget::Store,
                    tessaridb::VaultAct::Change {
                        current: &current,
                        new: &new,
                    },
                    tokens,
                    &presented,
                ),
                Err(shape) => Answer::bad_request(shape),
            },
            Err(refused) => refused,
        },
        (Method::POST, "/vault/unseal") => match body::text(&mut request) {
            Ok(body) => respond::vault::answer(
                db,
                tessaridb::VaultTarget::Store,
                tessaridb::VaultAct::Unseal {
                    passphrase: body.trim_end_matches('\n'),
                },
                tokens,
                &presented,
            ),
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
        // One vault carrying its own passphrase (ADR-0093 D6).
        (method, url)
            if url.starts_with("/vault/")
                && !matches!(url, "/vault/seal" | "/vault/unseal" | "/vault/passphrase") =>
        {
            respond::vault::one_vault(db, method, url, &mut request, tokens, &presented)
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
            | "/wire" | "/vault" | "/vault/seal" | "/vault/unseal" | "/vault/passphrase",
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
            None => console::asset(&method, url, request.header("If-None-Match"))
                .unwrap_or_else(|| Answer::new(404, r#"{"error":"no such route"}"#.to_owned())),
        },
    };

    // Counted here because this is the one place every answer this surface
    // writes passes through, which is what keeps "what a refusal is" a single
    // decision rather than one taken again at each route.
    stopping.answered(reply.status >= 400);
    if let Some(settled) = reply.settled {
        stopping.redirected(settled);
    }

    // Reported at the same single place, and at a level the status decides: a
    // 404 is traffic and a 500 is an event, and an operator filtering by level
    // should not have to know which routes produce which.
    if reply.status >= 500 {
        tracing::error!(request = id, status = reply.status, "request answered");
    } else if reply.status >= 400 {
        tracing::warn!(request = id, status = reply.status, "request answered");
    } else {
        tracing::info!(request = id, status = reply.status, "request answered");
    }

    reply
}

/// An answer as axum sends it, with the headers its status obliges.
pub(super) fn to_response(reply: Answer) -> Response {
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
    // Kept, but asked about every time: the tag is what makes the asking cheap,
    // and `no-cache` is what stops a browser giving an unhashed asset heuristic
    // freshness across a node upgrade (RFC 9111 §4.2.2, §5.2.2.4).
    if let Some(tag) = reply.tag
        && let Ok(value) = HeaderValue::from_str(tag)
    {
        headers.insert(header::ETAG, value);
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    // The answer says what it is; an answer without a content type still beats
    // no answer if a kind ever failed to be a header.
    if let Ok(value) = HeaderValue::from_str(reply.kind) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    response
}
