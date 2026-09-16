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
pub(crate) fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// One request, and everything that came back: status, headers, body.
pub(crate) fn get(address: &str, path: &str) -> (u16, Vec<String>, String) {
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
    let headers: Vec<String> = lines.map(str::to_owned).collect();
    let chunked = headers.iter().any(|line| {
        line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunked(body)
    } else {
        body.to_owned()
    };
    (status, headers, body)
}

/// A chunked body, put back together.
///
/// Not a nicety. `tiny-http` switches to `Transfer-Encoding: chunked` once a
/// response passes a size threshold, and the console's script crossed it — so
/// the tests that scan the served bytes had been scanning **chunk-size markers
/// mixed into the source**, and the first symptom was an id that read
/// `change-status\n14cf\n`. It was correct by accident while the file was small
/// and became wrong on a day that had nothing to do with the assertion.
///
/// Every scan of a served body in this file depends on this, which is why it
/// lives beside `get` rather than in the one test that noticed.
fn dechunked(body: &str) -> String {
    let mut rest = body;
    let mut out = String::with_capacity(body.len());
    while let Some((header, tail)) = rest.split_once("\r\n") {
        // A chunk header may carry extensions after a `;`; the size is the part
        // before it, in hex.
        let size = header.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size, 16) else {
            break;
        };
        if size == 0 || tail.len() < size {
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail.get(size.saturating_add(2)..).unwrap_or("");
    }
    out
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
/// Every script the page loads.
///
/// One of them now: the page is emitted from a bundler, so what used to be two
/// files reading each other's globals is one module graph. The slice stays,
/// rather than collapsing back into a literal at each call site, because the
/// defect it guards against is a test that reads one script and reports on all
/// of them — and that defect returns the day a second one is added.
#[cfg(feature = "console")]
const SCRIPTS: &[&str] = &["/console.js"];

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
        // A `data:` URI is not a place; it is the bytes themselves, written
        // where a reference would go. It satisfies F6 by construction rather
        // than by being fetched from this process — so it is the one form of
        // reference that is checked by *not* being followed. Excluding it is not
        // a hole in the property: a page made entirely of them would still work
        // with the network cut, which is the whole thing being protected.
        if url.starts_with("data:") {
            continue;
        }
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
    // **Every** script. This read `/console.js` alone until a route added in a
    // second file walked straight past it — a guard that covers half the console
    // is a guard that reports "no private door" about one door.
    let mut code = String::new();
    for script in SCRIPTS {
        let (status, _, held) = get(&address, script);
        assert_eq!(status, 200, "{script} is not served");
        code.push_str(&held);
    }

    let public = [
        "/script",
        "/session",
        "/password",
        "/watch",
        "/health",
        "/ready",
        "/metrics",
        "/backup",
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
         USE DATABASE library; DEFINE COLLECTION users; CREATE users:1 = { name: 'ada' }; \
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

// ─────────────────────────────────────────── presence, not only validity ──
//
// Everything above this line asks whether what the page POINTS AT is valid: is
// the URL served by this process, does the route already exist, is the request
// one this node answers. None of it asks whether what the page PROMISES is
// reachable, or whether what it SAYS is true.
//
// That distinction is not academic. The console once shipped, compiled, for two
// releases, telling operators the cluster was not built — while every test above
// stayed green, because the false text referenced no URL, called no route and
// changed no request. Four consecutive waves shipped a defect of that shape.
//
// The three tests below add the missing axis.

/// Every function in `panel/src/dom.ts` that takes an element id first.
///
/// The ids themselves are read out of the served script rather than listed
/// here, for the reason `quoted_urls` gives: a list is a second opinion about
/// what the file contains, and it agrees with the file right up until somebody
/// edits one of them. This names the HELPERS, which is a much smaller and much
/// slower-moving set.
///
/// The script used to reach the page through `document.getElementById` wrapped
/// in one helper, so one needle found every reach. The TypeScript port put the
/// typing behind a small vocabulary instead — `value("user")` rather than
/// `at("user").value` — and scanning for the old needle alone would still have
/// PASSED while covering a third of what it used to.
///
/// So the list is here, and `dom.ts` is where it comes from: a new id-taking
/// helper added there is added here, and the assertion below is what says so.
#[cfg(feature = "console")]
const REACHES: &[&str] = &[
    "at", "value", "trimmed", "setValue", "write", "clear", "hide", "disable", "say", "put",
    "facts",
];

/// Every element id the script reaches for, through any of `REACHES`.
#[cfg(feature = "console")]
fn addressed_ids(script: &str) -> Vec<String> {
    let mut found = Vec::new();
    for helper in REACHES {
        let needle = format!("{helper}(\"");
        let mut rest = script;
        while let Some(start) = rest.find(&needle) {
            rest = &rest[start.saturating_add(needle.len())..];
            match rest.find('"') {
                Some(end) => {
                    found.push(rest[..end].to_owned());
                    rest = &rest[end..];
                }
                None => break,
            }
        }
    }
    found
}

/// One tag's attribute values, for a tag body already cut out of the markup.
#[cfg(feature = "console")]
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)?.saturating_add(needle.len());
    let end = tag[start..].find('"')?.saturating_add(start);
    Some(tag[start..end].to_owned())
}

/// Every tag in `html`, as the text between `<` and the `>` that closes it.
///
/// A parser would be better and would be a dependency. This is enough for the
/// question being asked, because the attributes it reads are written by us, on
/// elements we control, and a malformed tag fails the tests rather than passing
/// them quietly.
#[cfg(feature = "console")]
fn tags(html: &str) -> Vec<&str> {
    html.split('<')
        .filter_map(|piece| piece.find('>').map(|end| &piece[..end]))
        .collect()
}

/// Every `id="…"` the page declares.
#[cfg(feature = "console")]
fn declared_ids(html: &str) -> std::collections::HashSet<String> {
    tags(html)
        .iter()
        .filter_map(|tag| attribute(tag, "id"))
        .collect()
}

#[cfg(feature = "console")]
#[test]
fn every_element_the_console_reaches_for_exists_on_the_page() {
    let (_node, address) = node();
    let (_, _, page) = get(&address, "/");
    let declared = declared_ids(&page);

    let mut addressed = Vec::new();
    for script in SCRIPTS {
        let (_, _, source) = get(&address, script);
        for id in addressed_ids(&source) {
            addressed.push((*script, id));
        }
    }

    assert!(
        !addressed.is_empty(),
        "no element ids were extracted at all — the scripts changed shape and \
         this test stopped asking its question, which is worse than failing"
    );

    let missing: Vec<String> = addressed
        .iter()
        .filter(|(_, id)| !declared.contains(id))
        .map(|(script, id)| format!("{script} reaches for #{id}, which the page does not declare"))
        .collect();

    assert!(
        missing.is_empty(),
        "the console addresses elements that do not exist:\n  {}",
        missing.join("\n  ")
    );
}

#[cfg(feature = "console")]
const CAPABILITIES: &[&str] = &[
    "cluster",
    "replica",
    "replication",
    "failover",
    "leader",
    "lease",
    // Widened in W318. The list began as the cluster vocabulary, because a false
    // cluster claim is what shipped in two published images. The defect is not
    // about clusters: it is a sentence claiming the engine does or does not do
    // something, and the console now says several such things about HISTORY —
    // which it is right about, and which therefore belongs on the record beside
    // everything else it is right about. Widening a scan is the cheap direction;
    // a narrow one goes on passing while its reach shrinks.
    "history",
    "timeline",
    "audit",
    // Widened again in W371, by the argument the paragraph above makes rather
    // than by a new one. W318 widened the vocabulary and left the corpus alone;
    // the reach shrank on the other axis, and the sentence that caught it said
    // neither `cluster` nor `history` but *role* and *statement* — it scored
    // zero here and would have gone on scoring zero however wide the corpus
    // grew. Measured before it was changed: `statement` adds one sentence and
    // `role` adds five, and all six are true statements about what the engine
    // does and does not do, which is precisely what this list is for.
    "role",
    "statement",
];

/// Words that turn a sentence about a capability into a claim it is absent.
#[cfg(feature = "console")]
const NEGATIONS: &[&str] = &[
    " not ",
    " no ",
    "never",
    "cannot",
    "can't",
    " yet ",
    "absent",
    "missing",
    "planned",
    "coming",
    "unavailable",
    "unsupported",
];

/// Every sentence the console shows that says a capability is not there.
///
/// This list is the point of the test and not its exception. A true negative
/// statement is honest and belongs on the page — "there is no sharding" is worth
/// more to an operator than silence. What is not acceptable is a negative
/// statement arriving without anyone noticing, which is how the console came to
/// tell two releases' worth of operators that the cluster was not built.
///
/// So every such sentence is written down here with the reason it is true, and
/// the test fails both ways: on a sentence that is not on this list, and on a
/// list entry the page no longer contains. The second half is what stops the
/// list becoming a rubber stamp — when the cluster surface is built, the two
/// entries about it must be deleted from here, and the test is what will insist.
#[cfg(feature = "console")]
const ACCEPTED_NEGATIONS: &[(&str, &str)] = &[
    (
        "A password reaches the node and goes no further: what is stored is a hash, so no plaintext reaches the log or any replica.",
        "True: the credential is hashed before storage and the hash is what replicates.",
    ),
    (
        "Membership is a record and not a control plane: a node learns its peers through the same log it replicates data with, so there is nothing to stand up beside the database.",
        "True: membership is stored as a record, so no separate coordination service exists.",
    ),
    // The entry that used to sit here said "This pane is not yet a cluster
    // surface", with a note to delete it when the map shipped. The map shipped
    // in W317, the sentence became false, and this test is what said so —
    // which is the whole reason the record carries a deletion condition rather
    // than a permanent blessing.
    (
        "No lag figure, and no leadership for other nodes.",
        "True: there is no follower loop, so a non-writing node's copy has no last collection to measure a lag from; and `cluster.lease` is this node's own lease, never a peer's.",
    ),
    // The two entries that stood here said the store recorded no events for any
    // of these, and cited Q-685's measurement. The measurement was of three READ
    // surfaces and the conclusion drawn from it was wrong (Q-739): every commit
    // has always written a log record carrying what it changed. `INFO FOR
    // HISTORY OF` made it askable in W372, the sentences became false, and this
    // test is what insisted — which is the whole reason the record carries a
    // deletion condition rather than a permanent blessing.
    // The catalog sentence beside this one — "Only a record has a history…" — is
    // deliberately NOT here. It says "nothing recorded", which carries no word
    // from `NEGATIONS`, so it is not a capability denial by this test's own
    // definition and a row for it would sit here going stale forever. Adding it
    // was tried, and the stale half of this test is what refused it.
    (
        "The history could not be read — the node refused it, or this build does not answer INFO FOR HISTORY.",
        "Not a claim about the engine at all: it is what the sheet says when the second question failed, and it deliberately does NOT say the record has no history. A refusal and an empty history are different facts and render differently — that separation is the point of the sentence, and the reason it is worded as an inability rather than an absence.",
    ),
    (
        "Nothing pulls a replica forward on a timer, so a node that is not writing has no last collection its copy could be measured from; and this node knows which lease it holds, never which lease somebody else holds.",
        "True: the same two absences as the heading above it, stated as the reason rather than as the claim. Recorded separately because the splitter ends a sentence at the strong tag, so a heading and its explanation are two entries.",
    ),
    // The entry that stood here claimed BOTH halves of Q-683 — a drain with no
    // statement and a hand-over with none — and W370 made the first half false.
    // This test is what said so, and only after W371 widened it: the sentence
    // had moved into the script, where nothing was reading. The half that is
    // still true is recorded below on its own, and the drain is gone from the
    // page because the control exists, never because it was blessed here.
    (
        "Handing leadership over is the other thing you would come here for, and it has no statement behind it yet — so this drawer does not offer a control that would compose nothing.",
        "True, and measured (Q-683): the grammar carries no HANDOVER, STEP DOWN or YIELD, so there is no statement a control could compose. Delete when one of those verbs exists. The drain half of this sentence was deleted in W371, when `DEFINE NODE ROLES NONE` gave it one.",
    ),
    // The six below arrived together in W371, when the corpus grew to every
    // delivered asset and the vocabulary grew by `role` and `statement`. None
    // of them is new prose: every one has been on the operator's screen for
    // waves, saying something true about what the engine does not do, outside
    // any scan. That is what a reach that shrinks looks like from the inside —
    // not a failing test, but a passing one with less and less under it.
    (
        "A password rotation does not touch a role, and a role correction does not invalidate a password.",
        "True: `ALTER USER … PASSWORD` and `ALTER USER … ROLES` are separate statements writing separate fields, so neither disturbs the other.",
    ),
    (
        "A user with no table grants is governed by their role; a user with one reaches exactly what they were granted.",
        "True: a table grant is not additive to the role — the first grant narrows the user to what it names, which is why the grant screen says so before it composes anything.",
    ),
    (
        "A peer's row cannot be amended from here: there is no ALTER REPLICA, a second DEFINE REPLICA is refused for the name already in use, and DROP REPLICA is refused while this node holds no leadership.",
        "True, and measured against a running clustered node (drawer.ts module doc): `ALTER` takes only NAMESPACE, USER or TABLE; the second DEFINE answers `the name rp:<n> is already in use`; the DROP answers `this node is in a cluster and holds no leadership`.",
    ),
    (
        "a role this build may not know.",
        "True: a typed role is sent as written rather than checked against a list held in the console, so the node's own refusal is what the operator sees. The alternative is a console that refuses a role the engine has and this build has not heard of.",
    ),
    (
        "If they hold no table grant yet this NARROWS them: a user with grants reaches exactly what they were granted, and nothing else their role would have allowed.",
        "True: the same rule as the row above, stated where the operator is about to act on it rather than where it is explained.",
    ),
    (
        "Declared with no roles — drained.",
        "True: an empty role set is the drained state and the engine's own source calls it that. It is a reading of what the node declares, not a claim that the state cannot be reached — W370 gave it the statement `DEFINE NODE ROLES NONE`.",
    ),
];

/// Element names that end a phrase whether or not it was punctuated.
///
/// Without this, a button label with no full stop is glued onto the front of the
/// next paragraph, and the resulting "sentence" changes whenever an unrelated
/// label does. The sentences this test records have to be stable against edits
/// that are nowhere near them.
#[cfg(feature = "console")]
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "header",
    "footer",
    "button",
    "label",
    "legend",
    "summary",
    "details",
    "td",
    "th",
    "tr",
    "caption",
    "option",
    "pre",
    "blockquote",
    "dt",
    "dd",
    "main",
    "nav",
    "article",
    "aside",
    "form",
    "fieldset",
    "span",
    "code",
    "strong",
];

/// Whether this tag body opens or closes one of the phrase-ending elements.
#[cfg(feature = "console")]
fn is_block_boundary(tag: &str) -> bool {
    let name: String = tag
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    // `span`, `code` and `strong` are inline, and breaking on them would cut a
    // real sentence in half — so they end a phrase only when the phrase has no
    // sentence punctuation in it at all, which is handled by the caller keeping
    // punctuated text flowing. Here the test is purely on the name.
    BLOCKS.contains(&name.as_str())
}

/// The page's visible prose, one phrase per entry.
#[cfg(feature = "console")]
fn sentences(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut tag = String::new();
    let mut inside_tag = false;

    let flush = |current: &mut String, out: &mut Vec<String>| {
        let phrase = current.trim().to_owned();
        if !phrase.is_empty() {
            out.push(phrase);
        }
        current.clear();
    };

    for character in html.chars() {
        match character {
            '<' => {
                inside_tag = true;
                tag.clear();
            }
            '>' if inside_tag => {
                inside_tag = false;
                if is_block_boundary(&tag) {
                    flush(&mut current, &mut out);
                } else if !current.ends_with(' ') && !current.is_empty() {
                    current.push(' ');
                }
            }
            _ if inside_tag => tag.push(character),
            c if c.is_whitespace() => {
                if !current.ends_with(' ') && !current.is_empty() {
                    current.push(' ');
                }
            }
            c => {
                current.push(c);
                if matches!(c, '.' | '!' | '?') {
                    flush(&mut current, &mut out);
                }
            }
        }
    }
    flush(&mut current, &mut out);

    out.into_iter()
        .map(|phrase| {
            phrase
                .replace("&mdash;", "\u{2014}")
                .replace("&amp;", "&")
                .replace("&nbsp;", " ")
        })
        .collect()
}

/// Every asset the console delivers, as this process answers for it.
///
/// `(path, content type, body)`, walked out of the page's own references for
/// the reason the URL test gives: a list written here is a second opinion that
/// agrees with the page right up until somebody edits one of them. Documents
/// are followed — the page and any stylesheet — and the script's URLs are not,
/// because those are API routes the browser calls rather than assets it fetches.
///
/// The content type comes back with the bytes because it is what decides how
/// prose is read out of them. Keying that off a filename extension would be
/// this test guessing at the bytes; the header is the process's own claim about
/// what it just served.
#[cfg(feature = "console")]
fn delivered(address: &str) -> Vec<(String, String, String)> {
    let mut queue: Vec<String> = vec!["/".to_owned()];
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<(String, String, String)> = Vec::new();

    while let Some(path) = queue.pop() {
        // A `data:` URI is the bytes themselves rather than a place to fetch
        // them from, and an absolute URL is somebody else's asset — neither is
        // something this process delivers. The URL test is what refuses the
        // second one; here it is simply not ours to read.
        if path.starts_with("data:") || path.contains("://") || seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let (status, headers, body) = get(address, &path);
        assert_eq!(
            status, 200,
            "the console references {path:?} and this node answers {status}, so \
             the browser asks for it and gets nothing"
        );
        let kind = header(&headers, "Content-Type")
            .unwrap_or_default()
            .to_owned();
        if kind.starts_with("text/html") || kind.starts_with("text/css") {
            queue.extend(quoted_urls(&body));
        }
        out.push((path, kind, body));
    }

    assert!(
        out.len() >= 2,
        "only {} asset(s) were walked, so this corpus is one file again and the \
         scan would pass against almost anything",
        out.len()
    );
    out
}

/// Split what one literal held into sentences, keeping those long enough to be
/// prose.
///
/// Three words is the floor. Below it a literal is a label, a class name or an
/// identifier, and a capability word inside one is a coincidence rather than a
/// claim about the engine.
#[cfg(feature = "console")]
fn keep_sentences(phrase: &mut String, out: &mut Vec<String>) {
    let text: String = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
    phrase.clear();

    let mut current = String::new();
    let flush = |current: &mut String, out: &mut Vec<String>| {
        let sentence = current.trim().to_owned();
        current.clear();
        if sentence.split_whitespace().count() >= 3 {
            out.push(sentence);
        }
    };
    for character in text.chars() {
        current.push(character);
        if matches!(character, '.' | '!' | '?') {
            flush(&mut current, out);
        }
    }
    flush(&mut current, out);
}

/// The prose inside a script's string literals, one sentence per entry.
///
/// A bundle's operator-facing words are all inside literals. Its comments ride
/// along in the delivered bytes but are never shown, so reading them would make
/// this test's own failure message — *"the console says a capability is
/// absent"* — false about every sentence it reported.
///
/// A template's `${…}` ends a phrase, because an interpolation is a value and
/// not a word anybody reads; leaving it in would also make the recorded
/// sentence change whenever the surrounding code did, which is the one thing a
/// record of sentences cannot afford.
///
/// Over-collection is the safe direction here: a phrase that is not really
/// prose surfaces as an unrecorded sentence and somebody looks at it. Missing
/// one is the silent direction, and is the defect this exists to close.
#[cfg(feature = "console")]
fn literals(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut phrase = String::new();
    let mut characters = source.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '/' if characters.peek() == Some(&'/') => {
                for skipped in characters.by_ref() {
                    if skipped == '\n' {
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                let mut star = false;
                for skipped in characters.by_ref() {
                    if star && skipped == '/' {
                        break;
                    }
                    star = skipped == '*';
                }
            }
            '"' | '\'' | '`' => {
                let quote = character;
                while let Some(inner) = characters.next() {
                    match inner {
                        // What an escape stood for is not readable prose, and a
                        // space keeps it from gluing the words on either side of
                        // it into one that is in no dictionary.
                        '\\' => {
                            phrase.push(' ');
                            characters.next();
                        }
                        held if held == quote => break,
                        '\n' if quote != '`' => break,
                        '$' if quote == '`' && characters.peek() == Some(&'{') => {
                            characters.next();
                            keep_sentences(&mut phrase, &mut out);
                            let mut depth = 1_usize;
                            for skipped in characters.by_ref() {
                                match skipped {
                                    '{' => depth = depth.saturating_add(1),
                                    '}' => {
                                        depth = depth.saturating_sub(1);
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        held => phrase.push(held),
                    }
                }
                keep_sentences(&mut phrase, &mut out);
            }
            _ => {}
        }
    }
    out
}

/// The sentences an operator can read in one delivered asset.
#[cfg(feature = "console")]
fn prose(kind: &str, body: &str) -> Vec<String> {
    if kind.starts_with("text/html") {
        sentences(body)
    } else {
        literals(body)
    }
}

/// Whether `sentence` says something about a capability being absent.
#[cfg(feature = "console")]
fn denies_a_capability(sentence: &str) -> bool {
    let padded = format!(" {} ", sentence.to_lowercase());
    CAPABILITIES.iter().any(|word| padded.contains(word))
        && NEGATIONS.iter().any(|word| padded.contains(word))
}

#[cfg(feature = "console")]
#[test]
fn every_sentence_that_denies_a_capability_is_on_the_record() {
    let (_node, address) = node();

    // Every asset, not the page. Until W371 this read `GET /` alone, so the
    // script's sentences — the bulk of what an operator is actually shown — were
    // outside the scan entirely, and the guard went on passing while its reach
    // shrank. The sentence that proved it was in the script.
    let shown: Vec<(String, String)> = delivered(&address)
        .into_iter()
        .flat_map(|(path, kind, body)| {
            prose(&kind, &body)
                .into_iter()
                .filter(|sentence| denies_a_capability(sentence))
                .map(move |sentence| (path.clone(), sentence))
        })
        .collect();

    let unrecorded: Vec<String> = shown
        .iter()
        .filter(|(_, sentence)| {
            !ACCEPTED_NEGATIONS
                .iter()
                .any(|(known, _)| *known == sentence.as_str())
        })
        .map(|(path, sentence)| format!("{path}: {sentence}"))
        .collect();

    assert!(
        unrecorded.is_empty(),
        "the console says a capability is absent, and nobody wrote down why it is \
         true. Add it to ACCEPTED_NEGATIONS with the reason, or fix the sentence:\n  {}",
        unrecorded.join("\n  ")
    );

    let stale: Vec<&str> = ACCEPTED_NEGATIONS
        .iter()
        .map(|(sentence, _)| *sentence)
        .filter(|sentence| !shown.iter().any(|(_, shown)| shown == sentence))
        .collect();

    assert!(
        stale.is_empty(),
        "ACCEPTED_NEGATIONS carries sentences the page no longer shows. A list \
         that outlives what it describes stops being a record and becomes a \
         rubber stamp — delete these:\n  {}",
        stale.join("\n  ")
    );
}

/// Every TypeScript module the console is built from, as the repository holds it.
///
/// Enumerated from the directory and never from a list written here. A list is
/// exactly the under-collection defect this band has now been bitten by twice:
/// a module added next month would not be on it, and the scan would go on
/// passing while its reach quietly shrank. The failure mode of reading the
/// directory is the loud one — a wrong path finds nothing — and the assertions
/// below turn that into a failure instead of a pass.
#[cfg(feature = "console")]
fn panel_sources() -> Vec<(String, String)> {
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/panel/src");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(directory).expect("the panel's source directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_none_or(|kind| kind != "ts") {
            continue;
        }
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        found.push((name, std::fs::read_to_string(&path).expect("a source file")));
    }
    found
}

/// The three modules that own a route, and the reason each one is allowed one.
///
/// `api.ts` is the only path to `/script` and records every statement it sends.
/// `password.ts` posts to `/password`, which is not a statement at all — a token
/// is not proof of a password — and records the action instead. `session.ts`
/// exchanges a password for a token and sends nothing on the store's behalf.
#[cfg(feature = "console")]
const MAY_REACH_THE_NODE: &[&str] = &["api.ts", "password.ts", "session.ts"];

#[cfg(feature = "console")]
#[test]
fn nothing_reaches_the_node_outside_the_modules_that_record_what_they_send() {
    // The gap this closes was MEASURED, not imagined: a `fetch("/script", …)`
    // planted in `users.ts` built, typechecked, and passed all fourteen console
    // tests. The log's most important property was a convention that nothing
    // anywhere checked, which is the same shape as the outage the previous wave
    // found — a property everyone believes and no mechanism verifies.
    let sources = panel_sources();
    assert!(
        sources.len() >= 10,
        "the panel's sources were not found, so this scan would pass by reading \
         nothing: {} file(s)",
        sources.len()
    );
    for expected in MAY_REACH_THE_NODE {
        assert!(
            sources.iter().any(|(name, _)| name == expected),
            "{expected} is not among the sources this test read, so the allow-list \
             is not describing the tree it is scanning"
        );
    }

    let reaching: Vec<&str> = sources
        .iter()
        .filter(|(name, text)| {
            !MAY_REACH_THE_NODE.contains(&name.as_str()) && text.contains("fetch(")
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        reaching.is_empty(),
        "these modules talk to the node without going through the one path that \
         records it: {reaching:?}"
    );

    // And the tighter half: `/script` is reachable from one module. A second
    // caller would be a mutation the statement log never hears about, which is
    // the whole failure this guards.
    let naming: Vec<&str> = sources
        .iter()
        .filter(|(name, text)| name != "api.ts" && text.contains("\"/script\""))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        naming.is_empty(),
        "these modules name the script route themselves rather than asking \
         `api.ts` for it: {naming:?}"
    );
}

#[cfg(feature = "console")]
#[test]
fn a_password_cannot_reach_the_statement_log_in_the_clear() {
    // Also measured rather than assumed: with the redaction deleted, every test
    // in this suite still passed, because nothing here had ever read a log
    // entry. This is a weaker check than a behavioural one and is recorded as
    // such — the redaction is a TypeScript rule and there is no test runner in
    // this repository that can call it. What it does assert is the shipped
    // artifact: the single statement that writes an entry applies the rule.
    let (_node, address) = node();
    let (status, _, script) = get(&address, "/console.js");
    assert_eq!(status, 200, "the console's script is not served");
    assert!(
        script.contains("unshift("),
        "nothing in the served script writes a log entry, so this test is \
         asserting about code that is not there"
    );

    let writes: Vec<&str> = script
        .lines()
        .filter(|line| line.contains("unshift("))
        .collect();
    for line in &writes {
        assert!(
            line.contains("redacted("),
            "a log entry is written without the redaction: {}",
            line.trim()
        );
    }

    // The rule itself, so a redaction narrowed back to its old shape — the last
    // occurrence only, no escape handling — fails here rather than in the field.
    assert!(
        script.contains("/PASSWORD\\s+'(?:[^'\\\\]|\\\\.)*'/gi"),
        "the redaction is not the global, escape-aware one the log relies on"
    );
}

#[cfg(feature = "console")]
#[test]
fn no_screen_but_run_asks_the_operator_to_read_a_statement() {
    // Criterion T2.4's own measurement, made on the served page rather than on
    // a claim about it. The user forms used to draw the statement they would
    // send; they now say what will happen. The one statement still on a screen
    // is the removal's, because what is confirmed has to be the thing that runs
    // — and it sits behind a disclosure, which makes it readable without making
    // it read.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");

    let previews: Vec<&str> = page
        .match_indices("id=\"")
        .filter_map(|(at, _)| {
            let rest = page.get(at.saturating_add(4)..)?;
            let id = rest.split('"').next()?;
            id.ends_with("-preview").then_some(id)
        })
        .collect();
    assert!(
        !previews.is_empty(),
        "no preview element was found at all, so the assertion below would pass \
         by finding nothing"
    );

    // A `pre` is the panel's component for a statement; a `p.says` is its
    // component for a consequence. Which one an id is drawn with is the whole
    // difference the criterion measures.
    for id in &previews {
        let at = page
            .find(&format!("id=\"{id}\""))
            .expect("the id just found");
        let opened = page[..at].rfind('<').expect("an opening tag");
        let tag = &page[opened..at];
        if !tag.starts_with("<pre") {
            continue;
        }
        assert_eq!(
            *id, "remove-preview",
            "a screen outside Run draws a statement for the operator to read"
        );
        // The NEAREST enclosing disclosure, not the first one on the page. The
        // first version took `<details class="statement">` as a stand-in for
        // *the disclosure holding this preview*, which held only while there
        // was one on the page — the cluster map's raw answer arrived behind a
        // second one, earlier in document order, and the assertion inverted.
        // The class is now named for the pattern rather than for a content it
        // once had, and this looks backwards from the element itself.
        let disclosure = page[..at]
            .rfind("<details class=\"tucked\">")
            .expect("the removal's statement sits behind a disclosure");
        let closed = page[disclosure..]
            .find("</details>")
            .map(|end| disclosure.saturating_add(end))
            .expect("the disclosure closes");
        assert!(
            at < closed,
            "the removal's statement is not inside the disclosure that should \
             hold it"
        );
    }
}

#[cfg(feature = "console")]
#[test]
fn the_four_destinations_are_named_for_the_jobs_and_there_are_four() {
    // S3.2's own measurement. Renaming the destinations passed every test this
    // suite had, because they all assert that a tab and a pane AGREE — which
    // stays true whatever the tab is called. The labels are the criterion, so
    // the labels are what this asserts, in order.
    //
    // The cap of four is asserted with them rather than separately: a fifth
    // destination is a trade to be argued for, and a test that only checked the
    // names would let one arrive silently beside them.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");

    let labels: Vec<String> = page
        .split("<button")
        .filter(|piece| piece.contains(r#"role="tab""#))
        .filter_map(|piece| {
            let opened = piece.find('>')?;
            let rest = piece.get(opened.saturating_add(1)..)?;
            rest.split('<').next().map(str::trim).map(str::to_owned)
        })
        .collect();

    assert_eq!(
        labels,
        vec!["Run", "Cluster", "Access", "This node"],
        "the destinations no longer name the jobs, or a fifth has arrived"
    );
}

#[cfg(feature = "console")]
#[test]
fn every_module_that_declares_a_wire_is_started_by_the_console() {
    // Measured, not imagined: `search.ts` was imported by `console.ts` and never
    // started. The bundler then dropped the whole module as unreachable, the
    // page kept the field and the shortcut hint it draws, and NOTHING reported
    // anything — not the typechecker, not the build, not a test. The console
    // simply had a search box that did not search.
    //
    // A module that exports `wire` is declaring that it has start-up work. This
    // asserts the declaration is honoured.
    let sources = panel_sources();
    assert!(
        sources.len() >= 10,
        "the panel's sources were not found, so this scan would pass by reading \
         nothing: {} file(s)",
        sources.len()
    );
    let entry = sources
        .iter()
        .find(|(name, _)| name == "console.ts")
        .map(|(_, text)| text.as_str())
        .expect("console.ts is not among the sources this test read");

    // The BINDING and not the file stem. `user-forms.ts` is imported as
    // `userForms`, so a scan keyed on the stem reports it unstarted while it is
    // started — the same needle-for-a-subject substitution these tests exist to
    // catch, and it went red here on the first run.
    let started: Vec<&str> = entry
        .lines()
        .filter_map(|line| line.trim().strip_suffix(".wire();"))
        .collect();
    let unstarted: Vec<String> = sources
        .iter()
        .filter(|(name, text)| name != "console.ts" && text.contains("export function wire("))
        .filter(|(name, _)| {
            let stem = name.trim_end_matches(".ts");
            let alias = entry
                .lines()
                .find(|line| line.contains(&format!("from \"./{stem}.js\"")))
                .and_then(|line| line.split(" as ").nth(1))
                .and_then(|rest| rest.split_whitespace().next());
            match alias {
                Some(alias) => !started.contains(&alias),
                None => true,
            }
        })
        .map(|(name, _)| name.clone())
        .collect();
    assert!(
        unstarted.is_empty(),
        "these modules declare start-up work that nothing starts, so the bundler \
         will drop them and the page will quietly lack the behaviour: {unstarted:?}"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_listing_bounds_what_it_renders() {
    // Measured in a browser on a 5 001-account store: 200 rows cost 3.1 ms and
    // 5 001 cost 193 ms, unstyled, on a fast machine. The ceiling is the
    // difference between a list and a stall, and nothing else in this suite
    // would notice it being deleted.
    //
    // Structural, and recorded as such — it asserts the served script still
    // bounds the slice it renders, not that a browser draws 200 rows. The
    // behavioural half is the browser pass, which is where the numbers above
    // came from.
    let (_node, address) = node();
    let (status, _, script) = get(&address, "/console.js");
    assert_eq!(status, 200, "the console's script is not served");

    let bound = script
        .lines()
        .find(|line| line.contains("var SHOWN"))
        .and_then(|line| line.split('=').nth(1))
        .and_then(|rest| rest.trim().trim_end_matches(';').parse::<usize>().ok())
        .expect("the listing declares no render ceiling");
    assert!(
        (1..=1000).contains(&bound),
        "the render ceiling is {bound}, which is not a ceiling"
    );
    assert!(
        script.contains("slice(0, SHOWN)"),
        "the ceiling is declared and not applied, which is the same as absent"
    );
}

#[cfg(feature = "console")]
#[test]
fn what_is_served_is_the_file_that_was_committed() {
    // The assertion the whole file needed and did not have. Every test here
    // scans the served bytes; none of them checked that the served bytes are
    // the asset. They were not: the script passed `tiny-http`'s chunking
    // threshold and the harness was handing the scans a body with chunk-size
    // markers embedded in it.
    //
    // That failure is INTERMITTENT BY CONSTRUCTION, which is why it belongs
    // here rather than in the one test that happened to catch it. Whether a
    // marker corrupts anything depends on where in the file it lands, so the
    // same defect turns a scan red on Tuesday and green on Wednesday after an
    // unrelated edit moves the offset. Comparing the whole body to the file is
    // the only form that does not depend on that luck.
    let (_node, address) = node();
    for (path, committed) in [
        ("/console.js", include_str!("../../assets/console.js")),
        ("/console.css", include_str!("../../assets/console.css")),
        ("/", include_str!("../../assets/index.html")),
    ] {
        let (status, _, served) = get(&address, path);
        assert_eq!(status, 200, "{path} is not served");
        assert_eq!(
            served.len(),
            committed.len(),
            "{path} came back {} bytes against {} on disk — the body is not the \
             file, so every scan in this file is scanning something else",
            served.len(),
            committed.len()
        );
        assert_eq!(served, committed, "{path} is not what the repository holds");
    }
}

#[cfg(feature = "console")]
#[test]
fn the_console_draws_no_field_the_engine_no_longer_answers() {
    // S4.2's shipping bar, and the reason it is a bar. `membership` could only
    // ever report `alone` — one variant in the type — so it said a node stood
    // alone while the write path fenced that same node for belonging to a
    // cluster. It was the first field this console printed on its cluster tab.
    //
    // The engine no longer answers it. This asserts the console does not draw
    // it from anywhere else, including from a literal somebody re-adds while
    // reading an older screenshot.
    // Scanned for the FIELD and not for the word. The first version forbade the
    // string `membership` anywhere in the served page, which was fine until the
    // cluster screen legitimately wrote "Declare the membership" in a heading —
    // and then the guard failed for a sentence rather than for a defect. The
    // subject is a field the panel reads off an answer, so the shapes below are
    // how it would be read, and prose is left alone.
    let sources = panel_sources();
    assert!(
        sources.len() >= 10,
        "the panel's sources were not found: {} file(s)",
        sources.len()
    );
    let reading: Vec<&str> = sources
        .iter()
        .filter(|(_, text)| {
            text.contains("[\"membership\"]")
                || text.contains("membership:")
                || text.contains(".membership")
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        reading.is_empty(),
        "these modules read `membership` off an answer the engine does not \
         carry it in: {reading:?}"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_map_draws_no_figure_the_engine_cannot_answer() {
    // S4.2. Two values are excluded for the same reason and it is not taste:
    // the engine has no answer, so any number on screen would be invented.
    //
    // REPLICATION LAG — there is no follower loop, so a non-writing node's copy
    // has no last collection to measure a lag from. The only honest value is
    // absent, and a lag figure is the single most reassuring thing a cluster
    // screen can display, which is exactly why it must not display one.
    //
    // PER-RANGE PLACEMENT — there is no sharding. Every node holding a
    // namespace holds all of it, and a placement diagram would describe a
    // topology the product does not have.
    //
    // Scanned on the SOURCE the map is built from rather than on the rendered
    // page, because a figure that is drawn only when a cluster is present would
    // not appear on a page served by a lone node — and the lone node is what
    // this suite has.
    let sources = panel_sources();
    assert!(
        sources.len() >= 10,
        "the panel's sources were not found: {} file(s)",
        sources.len()
    );
    let map = sources
        .iter()
        .find(|(name, _)| name == "map.ts")
        .map(|(_, text)| text.as_str())
        .expect("map.ts is not among the sources this test read");

    // The words are checked where they would be DRAWN — a string the map puts
    // on screen — and not in prose. The header above says `lag` and `sharding`
    // several times on purpose: a scan that forbade the word outright would
    // forbid explaining why it is forbidden.
    for forbidden in [
        "\"lag\"",
        "\"replication lag\"",
        "\"ranges\"",
        "\"placement\"",
    ] {
        assert!(
            !map.contains(forbidden),
            "the map draws {forbidden}, which the engine cannot answer"
        );
    }
}

#[cfg(feature = "console")]
#[test]
fn the_map_draws_three_lamps_and_the_lease_carries_its_expiry() {
    // Both halves were GAPS found by falsification, not by design: deleting the
    // `writable` lamp and deleting the lease's expiry each left every test in
    // this suite green. They are asserted together because they are the same
    // claim — the map says exactly what the engine says and no less.
    //
    // THREE LAMPS. A role is three independent bits, so there are eight
    // combinations and no taxonomy of leader / follower / standby to collapse
    // them into. Two lamps cannot distinguish a drained node from a serving
    // one that does not write, and that distinction is the operator's own drain.
    //
    // THE LEASE'S EXPIRY. `coordinating` says the node may stand for
    // leadership; `cluster.lease` says whether it holds it, and a lease is a
    // grant with a clock. A marker without one implies a permanence the engine
    // never promised — and the engine's source carries a note about an earlier
    // version that read the role where it should have read the lease.
    let sources = panel_sources();
    let map = sources
        .iter()
        .find(|(name, _)| name == "map.ts")
        .map(|(_, text)| text.as_str())
        .expect("map.ts is not among the sources this test read");

    for bit in ["serving", "writable", "coordinating"] {
        assert!(
            map.contains(&format!("name: \"{bit}\"")),
            "the map no longer draws a lamp for `{bit}`"
        );
    }
    // `letter: "` and not `letter:` — the bare form also matches the function's
    // own parameter and the call site that passes it, which counted five for
    // three lamps. A needle that catches the declaration and its plumbing is
    // measuring the wrong thing even when the number happens to be right.
    let lamps = map.matches("letter: \"").count();
    assert_eq!(lamps, 3, "the map draws {lamps} lamps rather than three");

    assert!(
        map.contains("holds the lease until"),
        "the lease marker no longer carries its expiry"
    );
}

#[cfg(feature = "console")]
#[test]
fn forming_the_cluster_is_one_transaction_and_one_button() {
    // T4.7, and the trap behind it was reproduced twice against a running node:
    //
    //   DEFINE REPLICA warsaw …;  → ok
    //   DEFINE REPLICA lisbon …;  → error: this node is in a cluster and holds
    //                               no leadership
    //
    // The first declaration makes the node clustered, which costs it `writable`,
    // which is what the second needs. A per-row Add button would build that
    // cliff into the interface and strand the operator halfway through a
    // membership, with no way forward and no way back.
    let sources = panel_sources();
    let form = sources
        .iter()
        .find(|(name, _)| name == "formation.ts")
        .map(|(_, text)| text.as_str())
        .expect("formation.ts is not among the sources this test read");

    assert!(
        form.contains("\"BEGIN;\"") && form.contains("\"COMMIT;\""),
        "the membership is no longer declared in one transaction"
    );

    // One submit control on the whole screen, asserted on the served page so a
    // per-row button added to the markup is caught wherever it is written.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");
    let buttons = page.matches("id=\"form-cluster").count();
    assert_eq!(
        buttons, 1,
        "the formation screen carries {buttons} declare controls; one membership \
         is one decision and one transaction"
    );
    assert!(
        !page.contains("id=\"peer-0-add") && !page.contains("Add peer"),
        "a per-row Add button builds the engine's own cliff into the interface"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_four_states_are_four_renderings_and_four_messages() {
    // S5.1's own falsification, made into a test: force a partial and a failure
    // and find the same words. Before this band `say(id, words, failed?)` was
    // one string and one boolean, so five situations rendered as two — and the
    // pair that collapsed was the expensive one, because an operator reading a
    // bounded page as the whole list concludes an account is absent when it is
    // merely off screen.
    let sources = panel_sources();
    let states = sources
        .iter()
        .find(|(name, _)| name == "states.ts")
        .map(|(_, text)| text.as_str())
        .expect("states.ts is not among the sources this test read");
    for kind in ["waiting", "empty", "partial", "wrong"] {
        assert!(
            states.contains(&format!("\"{kind}\"")),
            "the state vocabulary no longer carries `{kind}`"
        );
    }

    // Four DISTINCT renderings, asserted on the stylesheet the node serves.
    // Four classes that all resolved to the same declarations would satisfy the
    // vocabulary and none of the criterion.
    let (_node, address) = node();
    let (status, _, css) = get(&address, "/console.css");
    assert_eq!(status, 200, "the console's stylesheet is not served");
    let mut painted: Vec<String> = Vec::new();
    for kind in ["waiting", "empty", "partial", "wrong"] {
        let rule = format!(".is-{kind} {{");
        let at = css
            .find(&rule)
            .unwrap_or_else(|| panic!("the stylesheet draws no `.is-{kind}`"));
        let body = css[at.saturating_add(rule.len())..]
            .split('}')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        painted.push(body);
    }
    // `waiting` and `empty` may legitimately share a treatment — both are quiet
    // and neither is urgent. What must differ is the pair the criterion names:
    // partial from empty, and wrong from everything.
    assert_ne!(
        painted[1], painted[2],
        "`empty` and `partial` are drawn identically, which is the collapse this \
         criterion exists to prevent"
    );
    assert!(
        painted[3] != painted[0] && painted[3] != painted[1] && painted[3] != painted[2],
        "`wrong` is drawn like another state"
    );

    // And four distinct MESSAGES on the one path that has all four.
    let users = sources
        .iter()
        .find(|(name, _)| name == "users.ts")
        .map(|(_, text)| text.as_str())
        .expect("users.ts is not among the sources this test read");
    // The KINDS and not the call shape: `state("user-status"` misses every call
    // the formatter wrapped across lines, which is three of the four here. A
    // needle that depends on where a formatter chose to break a line is
    // measuring the formatter.
    let unspoken: Vec<&str> = ["waiting", "empty", "partial", "wrong"]
        .into_iter()
        .filter(|kind| !users.contains(&format!("\"{kind}\"")))
        .collect();
    assert!(
        unspoken.is_empty(),
        "the listing never reaches these states, though that screen has all \
         four: {unspoken:?}"
    );
}

#[cfg(feature = "console")]
#[test]
fn nothing_the_console_remembers_is_a_credential() {
    // S5.2 asks the operator's context to survive a reload, which means the
    // console writes something down for the first time. The line it must not
    // cross is a credential, and the guard is structural because the failure is
    // silent: a password in `sessionStorage` looks like nothing at all until
    // somebody opens the tab on a shared machine.
    let sources = panel_sources();
    let source = sources
        .iter()
        .find(|(name, _)| name == "context.ts")
        .map(|(_, text)| text.as_str())
        .expect("context.ts is not among the sources this test read");

    // The LIST and not the file. Scanning the whole module for `"password"`
    // matched the `type="password"` in its own header — the module explaining
    // why it refuses credentials tripped the check that they are refused.
    let opened = source
        .find("REMEMBERED: readonly string[] = [")
        .expect("context.ts declares no remembered set");
    let listed = &source[opened..][..source[opened..]
        .find("];")
        .expect("the remembered set does not close")];

    // Every id the panel draws as a password control, taken from the page
    // itself rather than from a list here — a list would be the thing that goes
    // stale the day a form gains a field.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");
    let secrets: Vec<String> = page
        .split('<')
        .filter(|tag| tag.starts_with("input") && tag.contains(r#"type="password""#))
        .filter_map(|tag| {
            let at = tag.find(r#"id=""#)?;
            tag.get(at.saturating_add(4)..)?
                .split('"')
                .next()
                .map(str::to_owned)
        })
        .collect();
    assert!(
        !secrets.is_empty(),
        "no password control was found on the page, so this scan would pass by \
         finding nothing"
    );
    for id in &secrets {
        assert!(
            !listed.contains(&format!("\"{id}\"")),
            "the console remembers `{id}`, which is a password control"
        );
    }

    // And the storage is per-tab. `localStorage` would hand tomorrow's reader
    // the namespace, the account name and the statement somebody was working on
    // tonight.
    assert!(
        source.contains("sessionStorage") && !source.contains("window.localStorage"),
        "the remembered context is not confined to the tab that typed it"
    );
}

#[cfg(feature = "console")]
#[test]
fn every_shortcut_the_console_answers_to_is_written_down_somewhere() {
    // S5.2 asks for reachable AND DISCOVERABLE. The console had three global
    // handlers scattered through three modules, each real and each known only
    // to whoever wrote it — which is reachable and not discoverable, and the
    // distinction is the whole criterion.
    //
    // The arm this guards against is deleting the HINT and keeping the handler:
    // a test that only checked the handler would stay green while the shortcut
    // became invisible again.
    let sources = panel_sources();
    let keys = sources
        .iter()
        .find(|(name, _)| name == "shortcuts.ts")
        .map(|(_, text)| text.as_str())
        .expect("shortcuts.ts is not among the sources this test read");

    // The list is data and it is rendered, so a shortcut with no row here is a
    // shortcut with no row in the sheet.
    for advertised in ["\"/\"", "⌘K", "⌘1", "⌘↵", "\"?\"", "Esc"] {
        assert!(
            keys.contains(advertised),
            "the key list no longer mentions {advertised}"
        );
    }

    // And a way in that is not itself a shortcut. A list you can only open with
    // a key is a joke played on exactly the person who needed the list.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");
    assert!(
        page.contains(r#"id="keys-open""#),
        "there is no control that opens the key list, so it is reachable only by \
         knowing the key that opens the list of keys"
    );
    assert!(
        page.contains(r#"id="keys-sheet""#),
        "the key list has nowhere to render"
    );
}

#[cfg(feature = "console")]
#[test]
fn nothing_invisible_rides_along_in_a_delivered_asset() {
    // The emitted assets are shipped inside the image, so a zero-width space or
    // a bidi control arriving in a copied string would be there for the life of
    // the build — and invisible in every review, because reading the file is
    // precisely the check that cannot find it.
    //
    // The joiners are NOT blanket-forbidden: U+200D is load-bearing in emoji
    // sequences and in Arabic, Indic, Thai and Hangul shaping, and a scan that
    // banned it outright would corrupt real text the day the console is
    // translated. What is forbidden is the set with no shaping role here.
    const INVISIBLE: &[(u32, &str)] = &[
        (0x200B, "zero-width space"),
        (0x2060, "word joiner"),
        (0xFEFF, "byte-order mark"),
        (0x00AD, "soft hyphen"),
        (0x115F, "Hangul choseong filler"),
        (0x1160, "Hangul jungseong filler"),
        (0x3164, "Hangul filler"),
        (0x180E, "Mongolian vowel separator"),
        (0x202A, "left-to-right embedding"),
        (0x202B, "right-to-left embedding"),
        (0x202D, "left-to-right override"),
        (0x202E, "right-to-left override"),
        (0x2066, "left-to-right isolate"),
        (0x2067, "right-to-left isolate"),
        (0x2068, "first strong isolate"),
    ];

    let (_node, address) = node();
    let mut carried: Vec<String> = Vec::new();
    for path in ["/", "/console.js", "/console.css", "/favicon.svg"] {
        let (status, _, served) = get(&address, path);
        assert_eq!(status, 200, "{path} is not served");
        assert!(
            !served.is_empty(),
            "{path} came back empty, so this scan would pass by reading nothing"
        );
        for (point, name) in INVISIBLE {
            if let Some(found) = char::from_u32(*point) {
                if served.contains(found) {
                    carried.push(format!("{path} carries {name} (U+{point:04X})"));
                }
            }
        }
        // Unicode tag characters smuggle arbitrary ASCII and have no legitimate
        // use in any of these files.
        if served
            .chars()
            .any(|c| ('\u{E0000}'..'\u{E0080}').contains(&c))
        {
            carried.push(format!("{path} carries Unicode tag characters"));
        }
    }
    assert!(
        carried.is_empty(),
        "invisible codepoints reached a delivered asset: {carried:?}"
    );
}

#[cfg(feature = "console")]
#[test]
fn explanation_does_not_stand_in_the_operators_way() {
    // The brief's fourth register rule: prose paragraphs are not a UI element,
    // and explanation lives behind a disclosure, in the docs, or nowhere. It was
    // measured before it was enforced — the page carried 986 words across 21
    // paragraphs, eleven of them over forty words and one at a hundred and
    // forty-one.
    //
    // Two exclusions, and both are the rule rather than exceptions to it:
    //
    // A `note warn` is a CONSEQUENCE and not an explanation. Putting the cost of
    // an irreversible action behind a disclosure is precisely backwards, and the
    // destructive warning is deliberately long and deliberately in the way.
    //
    // Anything inside a `<details>` is already where the rule says explanation
    // belongs, so it is not counted at all — the rule is about what stands
    // between an operator and their task, not about total words.
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");

    // Every `<details>…</details>` span removed, so what remains is the flow.
    let mut flow = String::with_capacity(page.len());
    let mut rest = page.as_str();
    while let Some(open) = rest.find("<details") {
        flow.push_str(&rest[..open]);
        let after = &rest[open..];
        match after.find("</details>") {
            Some(close) => rest = &after[close.saturating_add(10)..],
            None => {
                rest = "";
                break;
            }
        }
    }
    flow.push_str(rest);

    let mut wordy: Vec<(usize, String)> = Vec::new();
    let mut seen = 0_usize;
    for piece in flow.split("<p class=\"note") {
        let Some(body) = piece.split_once('>').map(|(_, rest)| rest) else {
            continue;
        };
        if piece.starts_with(" warn") || piece.starts_with("note warn") {
            continue;
        }
        let Some(text) = body.split("</p>").next() else {
            continue;
        };
        // Tags out, words counted.
        let mut plain = String::new();
        let mut inside = false;
        for character in text.chars() {
            match character {
                '<' => inside = true,
                '>' => inside = false,
                _ if !inside => plain.push(character),
                _ => {}
            }
        }
        let words = plain.split_whitespace().count();
        if words == 0 {
            continue;
        }
        seen = seen.saturating_add(1);
        if words > 40 {
            wordy.push((
                words,
                plain
                    .split_whitespace()
                    .take(9)
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
        }
    }
    assert!(
        seen >= 8,
        "only {seen} notes were found in the flow, so this would pass by reading \
         almost nothing"
    );
    assert!(
        wordy.is_empty(),
        "these explanations stand in the operator's way; put them behind a \
         disclosure or cut them: {wordy:?}"
    );
}

/// Every question the search field puts to the node carries its own selection.
///
/// W319 drove the field against a running node and found two of its five shapes
/// broken by the same cause. A bare `INFO FOR TABLE orders` answers *no namespace
/// selected* every single time, because each `/script` request is its own
/// session — so the table shape could never resolve at all. And `split(".", 2)`
/// on `prod.library.orders` kept the first two parts, matched a real database,
/// and opened a sheet headed *database · prod.library.orders* showing that
/// database's tables: a wrong answer, under a name nothing is called, reported
/// as a success.
///
/// Neither failed anything. A candidate that cannot resolve simply falls through
/// to the next, and a candidate that resolves the wrong thing looks exactly like
/// one that resolved the right thing. So the invariant is asserted on the source
/// instead: a candidate that names a table, a database or a record must carry
/// the `USE` that gives it a namespace and a database to be named in.
#[test]
fn every_question_the_search_field_puts_carries_its_own_selection() {
    let source = panel_sources()
        .into_iter()
        .find(|(name, _)| name == "search.ts")
        .map(|(_, body)| body)
        .expect("the search module");

    let candidates: Vec<String> = source
        .split("out.push({")
        .skip(1)
        .map(|piece| piece.split("land:").next().unwrap_or(piece).to_owned())
        .collect();

    // Two assertions before the one this test is for. Without them a rename of
    // `out.push` collects nothing and the loop below passes by running zero
    // times, which is the shape a scanning test fails in.
    assert_eq!(
        candidates.len(),
        5,
        "the field is a CLOSED list of five shapes — a record, a table, a \
         database, an account and a namespace; {} were found, so either a shape \
         was added without a decision or the scan stopped matching",
        candidates.len()
    );

    let mut selected = 0_usize;
    for candidate in &candidates {
        let needs = candidate.contains("INFO FOR TABLE")
            || candidate.contains("INFO FOR DATABASE")
            || candidate.contains("SELECT ");
        if !needs {
            continue;
        }
        selected = selected.saturating_add(1);
        assert!(
            candidate.contains("USE NAMESPACE") && candidate.contains("USE DATABASE"),
            "this candidate names something that lives inside a database and \
             carries no selection, so the node will answer `no namespace \
             selected` and the field will silently try the next shape: \
             {candidate}"
        );
    }
    assert!(
        selected >= 3,
        "only {selected} candidates were found to need a selection, and three do \
         — the record, the table and the database. A vocabulary change has made \
         this test stop looking at what it is for"
    );
}

/// No screen waits for a CLICK on its tab before reading what it draws.
///
/// The cluster map and the account list each listened for a click on their own
/// tab, and every other way in left them blank AND silent: the console's own
/// ⌘1…⌘4, the arrow keys its tablist provides, a link somebody shared, a plain
/// reload. W319 followed a link to `#cluster` and got an empty map with an empty
/// status beside it — which an operator has no way to tell from a cluster that
/// holds nothing.
///
/// `show` is the one place a destination becomes visible, so `onArrival` is the
/// one place a first reading belongs. The guard is that no screen names a tab at
/// all: `tabs.ts` and `ui.ts` BUILD the ids and are the two exceptions, and a
/// third module mentioning one is a screen reaching for the input again.
#[test]
fn no_screen_waits_for_a_click_on_a_tab_to_read_what_it_draws() {
    let sources = panel_sources();
    assert!(
        sources.len() >= 20,
        "only {} panel sources were read, so this scan is looking at almost \
         nothing",
        sources.len()
    );

    let naming: Vec<&String> = sources
        .iter()
        .filter(|(name, _)| name != "tabs.ts" && name != "ui.ts")
        .filter(|(_, body)| body.contains("tab-"))
        .map(|(name, _)| name)
        .collect();
    assert!(
        naming.is_empty(),
        "these modules name a tab, and the only reasons to are to build the id \
         (ui.ts) or to route (tabs.ts) — anything else is a screen listening for \
         a click it will not always get: {naming:?}"
    );

    let registered = sources
        .iter()
        .filter(|(_, body)| body.contains("onArrival("))
        .count();
    assert!(
        registered >= 3,
        "only {registered} modules mention the arrival hook, and three should — \
         the one that declares it and the two screens that read on arrival. \
         Either a screen stopped reading at all, or the hook was renamed and \
         this guard now watches nothing"
    );
}

#[cfg(feature = "console")]
#[test]
fn nothing_a_screen_drew_outlives_the_reason_it_stopped_being_true() {
    // W319 replaced a click listener with an arrival and made the arrival
    // ONE-SHOT, and W320 measured what that bought: reach a destination before
    // signing in, the read is refused, the registration is spent, and the screen
    // holds that refusal for the life of the tab. Signing in did not recover it.
    // Leaving and coming back did not recover it. Three screens were reachable
    // that way and one of them by following a shared `#cluster` link.
    //
    // Two properties keep it fixed, and neither is visible from any one module:
    // an arrival repeats, and everything that makes a drawn answer false says so.
    let sources = panel_sources();
    assert!(
        sources.len() >= 20,
        "the panel's sources were not found, so this scan would pass by reading \
         nothing: {} file(s)",
        sources.len()
    );
    let source = |wanted: &str| -> &str {
        sources
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("{wanted} is not among the panel's sources"))
    };

    // An arrival is a registration and not a coupon. Removing it from the
    // registry as it fires is the exact line that caused the defect.
    let tabs = source("tabs.ts");
    assert!(
        !tabs.contains("splice("),
        "tabs.ts removes an arrival as it fires, so a destination reached once \
         under a failing condition can never read again"
    );
    assert!(
        tabs.contains("export function hereAgain("),
        "tabs.ts offers no way to re-read the destination on screen, so news \
         that arrives while the reader is already standing there cannot reach it"
    );

    // The three kinds of news that make a drawn answer false. Each is a module
    // that changes something the screens read FROM, and each must say so; the
    // identity is listed separately because it changes in three places and
    // missing any one of them leaves one identity's data under another's name.
    for (module, why) in [
        ("session.ts", "the identity changed"),
        ("drawer.ts", "a role declaration landed"),
        ("formation.ts", "a membership declaration landed"),
    ] {
        assert!(
            source(module).contains("hereAgain()"),
            "{module} changes what the screens draw ({why}) and does not tell \
             them, so the panel reports the change beside the state it replaced"
        );
    }
    let announcements = source("session.ts").matches("hereAgain()").count();
    assert!(
        announcements >= 3,
        "session.ts announces an identity change {announcements} time(s); it \
         changes in three — signing in, signing out, and a session the node \
         stopped honouring — and a missed one leaves the previous identity's \
         answer on screen"
    );
}

#[cfg(feature = "console")]
#[test]
fn two_different_failures_never_render_as_one_message() {
    // Both halves were measured against a running node in W320, and both told
    // the operator something false rather than something incomplete.
    //
    // A stopped node reached the account list as the browser's own
    // `Failed to fetch`, glued to this panel's explanation that a listing is
    // answered to whoever administers the tenancy — so an outage read as a
    // permissions problem and sent the reader to check grants while the node
    // was down. And a follow the node REFUSED ended on `stopped`, the same word
    // to the byte that the Stop button writes, because the socket closes right
    // after the refusal and the close handler overwrote it.
    let sources = panel_sources();
    let source = |wanted: &str| -> &str {
        sources
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("{wanted} is not among the panel's sources"))
    };

    // A request that never arrived is its own kind, so a screen can tell the
    // two apart without reading the browser's wording.
    assert!(
        source("api.ts").contains("export class Unreachable"),
        "api.ts does not distinguish a request that never reached the node from \
         one the node refused, so every screen has to guess from the wording"
    );
    // The BRANCH and not the import. Asserting the name alone passed with the
    // branch deleted, because `Unreachable` still appeared at the top of the
    // file — a scan for a token that lives in two places tests the quieter one.
    // Measured: the falsification arm that removed the branch stayed green.
    assert!(
        source("users.ts").contains("failure instanceof Unreachable"),
        "the account list explains every failure as a question of authority, \
         including the ones where nothing answered at all"
    );

    // The node's own account of a refusal has to survive the close that follows
    // it. A close handler that writes unconditionally erases it.
    // The CLOSE HANDLER'S OWN CALL, for the same reason. `toldWhy` appears four
    // times in this module, so a scan for the name passed with the one use that
    // matters deleted — also measured, also by the arm that was supposed to
    // catch it. What is asserted is the shape of the decision: the words the
    // close writes are chosen, not fixed.
    let watch = source("watch.ts");
    assert!(
        watch.contains("stop(toldWhy"),
        "watch.ts closes a follow without asking whether the node already said \
         why, so a refusal and an operator's own stop render identically"
    );
    assert!(
        watch.contains("toldWhy = true"),
        "nothing in watch.ts ever records that the node gave a reason, so the \
         flag the close consults can only ever be false"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_log_hears_the_statements_that_never_left_the_building() {
    // S2.2 is that every statement the panel issues reaches the statement log,
    // and W321 measured the one case where it did not: the tally stood at
    // `Statements 2`, Run was pressed against a stopped node, and it stood at
    // `Statements 2` afterwards. `ask` throws when the request never arrives and
    // the line that records it is the line after the one that throws — so the
    // statement an operator reconstructing an incident most wants to find is
    // precisely the one the log drops.
    //
    // Pre-existing rather than new: the raw `fetch` rejection escaped the same
    // way before W320 gave it a name.
    let sources = panel_sources();
    let api = sources
        .iter()
        .find(|(name, _)| name == "api.ts")
        .map(|(_, text)| text.as_str())
        .expect("api.ts is not among the panel's sources");

    let entries = api.matches("record(").count();
    assert!(
        entries >= 2,
        "api.ts writes a log entry {entries} time(s); it needs one for the reply \
         it got and one for the request that never arrived, or the log's claim is \
         false in the case it exists for"
    );

    // And the thrown sentence must not repeat what the screens already say. Three
    // of them prefix "the node did not answer: " to whatever is thrown, and W320
    // shipped the collision: the Run screen rendered "the node did not answer:
    // the node did not answer — it may be stopped or unreachable".
    let prefixing: Vec<&str> = sources
        .iter()
        .filter(|(_, text)| text.contains("\"the node did not answer: \""))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        !prefixing.is_empty(),
        "no screen prefixes the unreachable sentence any more, so this check is \
         guarding a collision that can no longer happen — delete it or re-aim it"
    );
    assert!(
        !api.contains("Unreachable(\"the node did not answer"),
        "the thrown text opens with the same words {prefixing:?} prefix to it, so \
         the reader is told twice in one line"
    );
}

#[cfg(feature = "console")]
#[test]
fn both_of_the_stylesheet_s_mandatory_clamps_are_present() {
    // The console's design has two clamps on its dials, and they are mandatory
    // together: motion goes to zero when the reader asks for reduced motion, and
    // the geometry steps one notch out of the dense band below 768px, where the
    // pointer is a finger rather than a mouse.
    //
    // Only the first was ever built. W322 measured the console at 375×812: rows
    // 35px and pressables 38px, the DESKTOP dial, with nothing overflowing — so
    // there was no symptom to notice, and a pair written as one sentence was
    // ticked as one thing. A clamp with no failure mode is exactly the kind that
    // needs a test rather than a reviewer.
    let css = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/console.css"))
        .expect("the emitted stylesheet");

    assert!(
        css.contains("@media (prefers-reduced-motion: reduce)"),
        "the stylesheet no longer honours a reader who asked for reduced motion"
    );
    assert!(
        css.contains("@media (max-width: 48rem)"),
        "the stylesheet has no clamp below 768px, so the console renders at the \
         dense dial on a phone — measured at 35px rows and 38px pressables before \
         this clamp existed"
    );

    // And the clamp has to carry a floor, not merely exist. 44px is where the
    // two platform conventions agree, and it is well above the 24×24 minimum the
    // desktop dial already clears — a clamp that moved nothing would pass the
    // check above and change nothing on the screen.
    let clamp = css
        .split("@media (max-width: 48rem)")
        .nth(1)
        .expect("the clamp block");
    let floors = clamp.matches("min-height: 2.75rem").count();
    assert!(
        floors >= 3,
        "the clamp sets a touch floor {floors} time(s); buttons, tabs and fields \
         are each pressed with a finger below this width"
    );
}

/// The drawer's module, as the repository holds it.
#[cfg(feature = "console")]
fn drawer_source() -> String {
    panel_sources()
        .into_iter()
        .find(|(name, _)| name == "drawer.ts")
        .map(|(_, body)| body)
        .expect("the drawer's module")
}

#[cfg(feature = "console")]
#[test]
fn unticking_every_role_composes_a_drain_this_node_accepts() {
    let source = drawer_source();
    assert!(
        source.contains("DEFINE NODE ROLES NONE;"),
        "the drawer composes no drain, so unticking every role sends nothing — \
         which is what it did before the statement existed"
    );

    // The statement is not taken on the drawer's word. W307 shipped a console
    // that claimed something about the engine the engine did not do, and the
    // cheap guard against a repeat is to send the exact words the console would
    // send to a node that is actually running. A source-only assertion here
    // would pass just as well against a typo.
    let (_node, address) = node();
    let (status, body) = script(&address, "DEFINE NODE ROLES NONE;");
    assert_eq!(
        status, 200,
        "the node refused the drain the drawer composes: {body}"
    );
    assert!(
        !body.contains("error"),
        "the node answered the drawer's own drain with an error: {body}"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_drain_says_what_it_costs_before_it_says_what_it_sends() {
    // S2.1 on the destructive action of this screen. A drain takes the node out
    // of service, and a control that shows only the statement leaves the
    // operator to work out what the statement does — which is the thing they
    // came here least able to do.
    // Read out of `preview` rather than out of the file, because the property is
    // about which of the two functions says what: `change` composes the
    // statement, `preview` says the consequence, and a phrase found anywhere in
    // the module would prove neither.
    let source = drawer_source();
    let radius = source
        .split_once("function preview(")
        .map(|(_, rest)| rest)
        .expect("the preview function")
        .split_once("export function show(")
        .map(|(body, _)| body)
        .expect("the function after it");
    assert!(
        !radius.contains("DEFINE NODE"),
        "the drain's preview renders a statement, so what the operator reads \
         before the button is the words that will run rather than what they do"
    );
    assert!(
        radius.contains("stops answering clients"),
        "the drain composes a statement without naming what it costs first"
    );
    assert!(
        radius.contains("keeps its data"),
        "the drain's radius does not say what survives it, so an operator \
         reading it cannot tell a drain from a removal"
    );

    // The one way the statement does not stick. On a node a membership row
    // declares a role for, a local drain is an override the next open discards
    // — so an operator who drains it and walks away has done nothing that
    // survives, and nothing anywhere would tell them.
    assert!(
        radius.contains("the next open discards"),
        "the drain does not warn that a membership declaration outlives it"
    );
}

#[cfg(feature = "console")]
#[test]
fn a_peer_offers_no_drain_because_no_statement_drains_a_peer() {
    // The refusal path, which is the one worth testing: a drain that works on
    // this node and silently composes nothing on a peer would look identical
    // until somebody needed it. `DEFINE NODE` reaches the local keyspace, so a
    // peer is drained by rewriting the membership row that declares it.
    let source = drawer_source();
    let (guard, _) = source
        .split_once("DEFINE NODE ROLES NONE;")
        .expect("the drain statement");
    let composed = guard
        .rsplit_once("export function change")
        .map(|(_, rest)| rest)
        .expect("the composing function");
    assert!(
        composed.contains("!subject.self"),
        "the drawer composes a statement for a subject it never checked is this \
         node, so a peer's drawer would send this node's drain"
    );

    let (_node, address) = node();
    let (_, _, page) = get(&address, "/");
    assert!(
        page.contains("drawer-apply"),
        "the drawer has no apply button, so this test read the wrong page"
    );
    assert!(
        drawer_source().contains(r#"hide("drawer-apply", !subject.self)"#),
        "the apply button is offered on a peer, where nothing it could send exists"
    );
}
