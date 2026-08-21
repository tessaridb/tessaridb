//! An HTTP surface for bgv-db.
//!
//! Two routes: one that runs a script, and one that says whether the node is
//! alive. Not a REST resource tree over tables — that would be a second query
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
//! # Not authenticated
//!
//! There is no identity layer yet, so **every request can do everything**. Said
//! here in as many words, because an endpoint that looks protected and is not is
//! worse than one that is obviously open. Bind it to a loopback address, or put
//! something in front of it, until that changes.

#![forbid(unsafe_code)]

mod json;
mod respond;

use std::sync::Arc;

use bgv_db::Db;
use tiny_http::{Method, Request, Response, Server};

pub use respond::Answer;

/// A node listening for HTTP requests.
pub struct Node {
    db: Arc<Db>,
    server: Server,
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
            server: Server::http(address)?,
        })
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

    /// Serve requests until the process ends.
    ///
    /// Each request is handled on its own thread and in its own session: no
    /// cookies, no connection state, no `USE` that outlives a request. A script
    /// says what it operates on, and session state across requests is
    /// authentication's problem rather than something to invent half of here.
    pub fn serve(&self) {
        for request in self.server.incoming_requests() {
            let db = Arc::clone(&self.db);
            // A panic in one request must not take the listener with it, and a
            // thread is what gives that for free.
            std::thread::spawn(move || {
                answer(&db, request);
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
        answer(&self.db, request);
        Ok(())
    }
}

/// Route one request and write its answer.
fn answer(db: &Db, mut request: Request) {
    let route = (request.method().clone(), request.url().to_owned());
    let reply = match (&route.0, route.1.as_str()) {
        (Method::Get, "/health") => respond::health(db),
        (Method::Post, "/script") => {
            let mut script = String::new();
            match request.as_reader().read_to_string(&mut script) {
                Ok(_) => respond::script(db, &script),
                Err(_) => Answer::bad_request("the request body is not text"),
            }
        }
        // "No such thing" and "not that way" are different answers, and a caller
        // debugging a client needs to know which one it got.
        (_, "/script" | "/health") => Answer::new(
            405,
            r#"{"error":"that route takes another method"}"#.to_owned(),
        ),
        _ => Answer::new(404, r#"{"error":"no such route"}"#.to_owned()),
    };

    let mut response = Response::from_string(reply.body).with_status_code(reply.status);
    // Every body here is JSON, so the header is a constant that parses — and if
    // it somehow did not, an answer without a content type still beats no answer.
    if let Ok(header) = "Content-Type: application/json".parse::<tiny_http::Header>() {
        response = response.with_header(header);
    }
    // A client that hung up mid-answer is not this node's problem, and there is
    // nobody left to tell.
    drop(request.respond(response));
}
