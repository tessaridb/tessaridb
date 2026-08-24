//! The console's bytes, and the routes that hand them over.
//!
//! Embedded rather than fetched, so a node on a network with no route out still
//! has an interface — which is the whole reason this exists rather than a
//! redirect to something hosted (ADR-0017).
//!
//! The assets are **committed**, so `cargo build` needs nothing but cargo, and
//! they are listed **by hand** below rather than swept up by a macro that walks
//! a directory: at three files a table is smaller than the dependency, and a
//! path that reaches one named thing is the same rule the rest of this surface
//! follows.
//!
//! The console gets no private route. Everything it does, it does through
//! `POST /script` and `GET /watch`, which is what keeps a console feature from
//! becoming a capability only the console has.

use tiny_http::Method;

use crate::respond::Answer;

/// One asset: the path it is served at, its type, and its bytes.
type Asset = (&'static str, &'static str, &'static str);

/// Everything the console is made of.
///
/// `include_str!` rather than `include_bytes!` because all three are text, and
/// text is what a reader of this file can check against the served answer.
const ASSETS: &[Asset] = &[
    (
        "/",
        "text/html; charset=utf-8",
        include_str!("../assets/index.html"),
    ),
    (
        "/console.css",
        "text/css; charset=utf-8",
        include_str!("../assets/console.css"),
    ),
    (
        "/console.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/console.js"),
    ),
];

/// The console asset served at `path`, if the console serves one there.
///
/// Reading is the only thing this answers: a page is fetched, never posted to,
/// and a method that is not `GET` falls through to the same "no such route" a
/// misspelt path gets.
pub(crate) fn asset(method: &Method, path: &str) -> Option<Answer> {
    if *method != Method::Get {
        return None;
    }
    ASSETS.iter().find_map(|(at, kind, body)| {
        (*at == path).then(|| Answer::text(200, (*body).to_owned(), kind))
    })
}

#[cfg(test)]
mod tests {
    use super::ASSETS;

    #[test]
    fn every_asset_the_page_references_is_one_this_module_serves() {
        // The obligation runs this way round on purpose: the page names what it
        // needs, and the table has to satisfy it. Reading the table and then
        // checking the page mentions each entry would pass while the page
        // referenced a fourth file nobody embedded.
        let page = ASSETS
            .iter()
            .find_map(|(at, _, body)| (*at == "/").then_some(*body))
            .expect("the console serves a root page");
        let mut referenced = 0;
        for quoted in page.split('"') {
            if !quoted.starts_with('/') || quoted.len() < 2 {
                continue;
            }
            referenced += 1;
            assert!(
                ASSETS.iter().any(|(at, _, _)| *at == quoted),
                "the page references {quoted:?}, which nothing here serves, so \
                 the browser asks this node for it and gets a 404"
            );
        }
        assert!(
            referenced >= 2,
            "the page referenced {referenced} local assets, so this test found \
             nothing to check and would pass against an empty page"
        );
    }
}
