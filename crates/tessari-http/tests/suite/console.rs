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
    (
        "Nothing pulls a replica forward on a timer, so a node that is not writing has no last collection its copy could be measured from; and this node knows which lease it holds, never which lease somebody else holds.",
        "True: the same two absences as the heading above it, stated as the reason rather than as the claim. Recorded separately because the splitter ends a sentence at the strong tag, so a heading and its explanation are two entries.",
    ),
    (
        "Draining this node and handing leadership over are the other two things you would come here for, and neither has a statement behind it yet — so this drawer does not offer a control that would compose nothing.",
        "True, and measured (Q-682, Q-683): `ROLES NONE` and `ROLES ;` are parse errors, there is no `DRAIN`, omitting `ROLES` means leave them alone, and the grammar carries no HANDOVER / STEP DOWN / YIELD. Delete when either verb exists.",
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
