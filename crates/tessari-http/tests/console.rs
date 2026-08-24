//! What an operator gets at `/`, and what it costs them to have it.
//!
//! Criterion F6 asks for the page to load and run a query **with every outbound
//! network path unavailable**. A cargo test cannot cut the machine's network,
//! and a test that claimed to have would be worse than one that does not try.
//!
//! So the property is proven where it actually lives: in the bytes that are
//! served. "Needs no route out" is a property of what the page references, not
//! of the environment it is opened in — and checking the bytes is *stronger*
//! than cutting the network, because it also catches a reference to a local
//! asset the binary forgot to embed, which no amount of network isolation would
//! reveal.
//!
//! What no test here does is **execute** the JavaScript. The page loads, every
//! byte it references comes from this process, and the exact request its script
//! builds is one this node answers — but that a browser running it draws the
//! right screen is not shown, and closing that needs a headless browser.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

/// A node on a loopback port the operating system picked, plus its address.
fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// One request, and everything that came back: status, headers, body.
fn get(address: &str, path: &str) -> (u16, Vec<String>, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, lines.map(str::to_owned).collect(), body.to_owned())
}

/// Post a script and answer with the status and the body.
#[cfg(feature = "console")]
fn script(address: &str, source: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "POST /script HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{source}",
        source.len()
    )
    .unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, body.to_owned())
}

/// The value of `field`, if the answer carried it.
#[cfg(feature = "console")]
fn header<'a>(headers: &'a [String], field: &str) -> Option<&'a str> {
    headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(field)
            .then(|| value.trim())
    })
}

/// Everything in `text` that sits inside double quotes and looks like a URL.
///
/// Extracted from the served bytes rather than from a list written here: a list
/// is a second opinion about what the page contains, and it agrees with the page
/// right up until somebody edits one of them.
#[cfg(feature = "console")]
fn quoted_urls(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .filter(|quoted| quoted.starts_with('/') || quoted.contains("://"))
        .map(str::to_owned)
        .collect()
}

#[cfg(feature = "console")]
#[test]
fn the_root_path_is_a_page_this_process_serves() {
    let (_node, address) = node();
    let (status, headers, body) = get(&address, "/");

    assert_eq!(status, 200, "the console did not answer at the root path");
    // The content type is not decoration: with the wrong one a browser shows the
    // source of the page instead of the page, and a status-only test passes.
    assert!(
        header(&headers, "Content-Type").is_some_and(|kind| kind.starts_with("text/html")),
        "the page was served as {:?}, so a browser renders it as text",
        header(&headers, "Content-Type")
    );
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html"),
        "the root answer is not an HTML document: {}",
        &body[..body.len().min(120)]
    );
}

#[cfg(feature = "console")]
#[test]
fn every_url_the_console_references_is_served_by_this_process() {
    // The check that makes F6 mean something. A page that redirects, or one that
    // pulls a script from a CDN, fails here — and so does one that references a
    // local file nobody embedded, which cutting the network would never reveal.
    let (_node, address) = node();
    let (_, _, page) = get(&address, "/");

    // The source set is read out of the page itself, never from a list written
    // here — a list is a second opinion that agrees with the page right up until
    // somebody edits one of them.
    let mut assets = quoted_urls(&page);
    let mut seen: Vec<String> = Vec::new();
    let mut checked = 0;
    while let Some(url) = assets.pop() {
        if seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        assert!(
            !url.contains("://"),
            "the console references {url:?}, which is somewhere else — the page \
             then needs a route out, which is the whole property F6 is about"
        );
        let (status, _, body) = get(&address, &url);
        assert_eq!(
            status, 200,
            "the console references {url:?} and this node answers {status} for \
             it, so the browser asks and gets nothing"
        );
        checked += 1;
        // A stylesheet can pull in further assets, so it is followed. The
        // script's URLs are API routes rather than things the browser fetches as
        // documents, and they answer on their own terms — the socket route
        // refuses a plain `GET` with 426, which is correct and is not a missing
        // asset. Their existence is the next test's job.
        if url.ends_with(".css") {
            assets.extend(quoted_urls(&body));
        }
    }
    assert!(
        checked >= 2,
        "only {checked} references were found in the page, so this test checked \
         almost nothing and would pass against a blank document"
    );

    // The script's references are not fetched here, but they must still point at
    // this origin: a script that *calls* somewhere else takes the property away
    // exactly as surely as a `<script src>` pointing there would.
    let (_, _, code) = get(&address, "/console.js");
    for url in quoted_urls(&code) {
        assert!(
            !url.contains("://"),
            "the console's script reaches {url:?}, so the page needs a route out \
             after all"
        );
    }
}

#[cfg(feature = "console")]
#[test]
fn the_console_calls_no_route_that_did_not_already_exist() {
    // ADR-0017 §4: anything the console can do, a `curl` can do. The cheapest
    // way to keep that true is to have no private path to offer — so the paths
    // the script names are checked against the public ones by hand, because a
    // new one appearing here is exactly the violation.
    let (_node, address) = node();
    let (_, _, code) = get(&address, "/console.js");

    let public = [
        "/script", "/watch", "/health", "/ready", "/metrics", "/backup",
    ];
    for url in quoted_urls(&code) {
        // The console's own assets are answered above; what matters here is the
        // API it reaches.
        if url == "/" || url.ends_with(".css") || url.ends_with(".js") {
            continue;
        }
        assert!(
            public.contains(&url.as_str()),
            "the console calls {url:?}, which is not one of the routes every \
             other client already has — a console-only route is the violation"
        );
    }
    assert!(
        code.contains("\"/script\"") && code.contains("\"/watch\""),
        "the console reaches neither the script route nor the watch route, so \
         this test has nothing to constrain"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_request_the_console_builds_is_one_this_node_answers() {
    // The half of "can run a query" that is provable without a browser: the
    // route and method the script names are read out of the script, and the same
    // request is then made from here.
    let (_node, address) = node();
    let (_, _, code) = get(&address, "/console.js");
    assert!(
        code.contains(r#"method: "POST""#) && code.contains(r#""/script""#),
        "the console no longer posts to the script route, so what this test \
         issues below is not what the page does"
    );

    let (status, body) = script(
        &address,
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE library; \
         USE DATABASE library; DEFINE TABLE users; CREATE users:1 = { name: 'ada' }; \
         SELECT * FROM users;",
    );
    assert_eq!(status, 200, "the console's own request was refused: {body}");
    assert!(
        body.contains("ada"),
        "the answer the console would render does not hold what was written: {body}"
    );
}

#[cfg(not(feature = "console"))]
#[test]
fn a_build_without_the_console_serves_no_page_at_all() {
    // Not a blank page and not an empty 200: the path simply is not there, which
    // is what tells an operator the binary was built without it rather than that
    // the console is broken.
    let (_node, address) = node();
    let (status, _, _) = get(&address, "/");
    assert_eq!(status, 404, "a build with no console still answered at `/`");
    let (status, _, _) = get(&address, "/console.js");
    assert_eq!(
        status, 404,
        "a build with no console still served its script"
    );
}
