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
/// Every script the page loads.
///
/// Named once here rather than at each call site, because the defect this
/// guards against is a test that reads one of them and reports on both.
#[cfg(feature = "console")]
const SCRIPTS: &[&str] = &["/console.js", "/sections.js"];

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
    // **Both** scripts. This read `/console.js` alone until a route added in
    // `sections.js` walked straight past it — a guard that covers half the
    // console is a guard that reports "no private door" about one door.
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

/// The id of every element the console's scripts reach for.
///
/// Read out of the served script rather than listed here, for the reason
/// `quoted_urls` gives: a list is a second opinion about what the file contains,
/// and it agrees with the file right up until somebody edits one of them.
#[cfg(feature = "console")]
fn addressed_ids(script: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = script;
    while let Some(start) = rest.find("at(\"") {
        rest = &rest[start.saturating_add(4)..];
        match rest.find('"') {
            Some(end) => {
                found.push(rest[..end].to_owned());
                rest = &rest[end..];
            }
            None => break,
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
#[test]
fn every_tab_and_every_pane_name_each_other() {
    let (_node, address) = node();
    let (_, _, page) = get(&address, "/");

    // A tab claims a pane with `aria-controls`; the pane claims the tab back
    // with `aria-labelledby`. Either one alone can be right while the other is
    // stale, and a pane no tab reaches is unreachable without being broken —
    // which is exactly the shape that ships quietly.
    let mut from_tabs = Vec::new();
    let mut from_panes = Vec::new();
    for tag in tags(&page) {
        let Some(role) = attribute(tag, "role") else {
            continue;
        };
        let Some(id) = attribute(tag, "id") else {
            continue;
        };
        match role.as_str() {
            "tab" => {
                if let Some(pane) = attribute(tag, "aria-controls") {
                    from_tabs.push((id, pane));
                }
            }
            "tabpanel" => {
                if let Some(tab) = attribute(tag, "aria-labelledby") {
                    from_panes.push((tab, id));
                }
            }
            _ => {}
        }
    }

    assert!(
        !from_tabs.is_empty(),
        "no tabs were found — the navigation changed shape and this test stopped \
         asking its question, which is worse than failing"
    );

    from_tabs.sort();
    from_panes.sort();
    assert_eq!(
        from_tabs, from_panes,
        "a tab and a pane disagree about each other. Left: what the tabs claim \
         to control. Right: what the panes claim to be labelled by. A pane that \
         appears on neither side is registered nowhere and is reachable by \
         nobody."
    );
}

/// Words that name something this engine either does or does not do.
#[cfg(feature = "console")]
const CAPABILITIES: &[&str] = &[
    "cluster",
    "replica",
    "replication",
    "failover",
    "leader",
    "lease",
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
    (
        "This pane is not yet a cluster surface.",
        "True while the pane draws facts rather than managing anything. Delete when the cluster map ships (G026 S4).",
    ),
    (
        "Nothing here manages anything: roles, failover, standby nodes, data placement and a live map of cluster state are being designed, and are deliberately absent rather than stubbed.",
        "True while G026 S4 is open. Delete when the map and the node drawer ship.",
    ),
    (
        "Nor does it draw lag or leadership for other nodes — nothing pulls a replica forward on a timer, so a node that is not writing has no last collection its copy could be measured from, and a number invented here would be the dashboard drawn ahead of the engine that makes the rest of this console untrustworthy.",
        "True: there is no follower loop, so per-node lag has no honest value.",
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
    let (_, _, page) = get(&address, "/");

    let shown: Vec<String> = sentences(&page)
        .into_iter()
        .filter(|s| denies_a_capability(s))
        .collect();

    let unrecorded: Vec<&String> = shown
        .iter()
        .filter(|sentence| {
            !ACCEPTED_NEGATIONS
                .iter()
                .any(|(known, _)| *known == sentence.as_str())
        })
        .collect();

    assert!(
        unrecorded.is_empty(),
        "the console says a capability is absent, and nobody wrote down why it is \
         true. Add it to ACCEPTED_NEGATIONS with the reason, or fix the sentence:\n  {}",
        unrecorded
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let stale: Vec<&str> = ACCEPTED_NEGATIONS
        .iter()
        .map(|(sentence, _)| *sentence)
        .filter(|sentence| !shown.iter().any(|shown| shown == sentence))
        .collect();

    assert!(
        stale.is_empty(),
        "ACCEPTED_NEGATIONS carries sentences the page no longer shows. A list \
         that outlives what it describes stops being a record and becomes a \
         rubber stamp — delete these:\n  {}",
        stale.join("\n  ")
    );
}
