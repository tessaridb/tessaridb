//! The console's bytes, and the routes that hand them over.
//!
//! Embedded rather than fetched, so a node on a network with no route out still
//! has an interface — which is the whole reason this exists rather than a
//! redirect to something hosted (ADR-0017).
//!
//! The assets are **committed**, so `cargo build` needs nothing but cargo, and
//! they are listed **by hand** below rather than swept up by a macro that walks
//! a directory: at four files a table is smaller than the dependency, and a
//! path that reaches one named thing is the same rule the rest of this surface
//! follows.
//!
//! The console gets no private route. Everything it does, it does through
//! `POST /script` and `GET /watch`, which is what keeps a console feature from
//! becoming a capability only the console has.

use std::sync::LazyLock;

use axum::http::Method;

use crate::respond::Answer;

/// One asset: the path it is served at, its type, and its bytes.
type Asset = (&'static str, &'static str, &'static str);

/// Everything the console is made of.
///
/// `include_str!` rather than `include_bytes!` because all four are text, and
/// text is what a reader of this file can check against the served answer.
///
/// One script, not two. `sections.js` used to sit beside `console.js` and read
/// its top-level names out of the global scope; the page is now emitted from a
/// bundler, so the two are one module graph with real imports and there is no
/// global coupling left to break.
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
    // Named by the page, so a browser asks for this instead of `/favicon.ico`
    // and the 404 that would otherwise sit in every operator's console.
    (
        "/favicon.svg",
        "image/svg+xml",
        include_str!("../assets/favicon.svg"),
    ),
];

/// Each asset's strong tag, in `ASSETS` order: a hash of its bytes, taken once.
///
/// The bytes are fixed for the life of the process, so a tag computed per
/// request would be the same answer paid for again; and the bytes are the
/// only thing a tag may follow, because a strong tag changes exactly when the
/// representation does (RFC 9110 §8.8.3) — a build number would change without
/// the page changing, and stay put on a dev build whose page did.
static TAGS: LazyLock<Vec<String>> = LazyLock::new(|| {
    ASSETS
        .iter()
        .map(|(_, _, body)| {
            // FNV-1a, 64 bits: stable across builds and toolchains, which the
            // standard library's hasher does not promise.
            let hash = body.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
            format!("\"{hash:016x}\"")
        })
        .collect()
});

/// The console asset served at `path`, if the console serves one there.
///
/// Reading is the only thing this answers: a page is fetched, never posted to,
/// and a method that is neither `GET` nor `HEAD` falls through to the same "no
/// such route" a misspelt path gets. A `HEAD` is answered as its `GET` is and
/// the body is dropped on the way out, so the two cannot disagree on a header.
///
/// A caller holding the current bytes — its `If-None-Match` names this tag — is
/// told `304` with no body, which is what makes `no-cache` cheap to live with.
pub(crate) fn asset(method: &Method, path: &str, if_none_match: Option<&str>) -> Option<Answer> {
    if *method != Method::GET && *method != Method::HEAD {
        return None;
    }
    // Split on `?` for the same reason `/backup` does: a query string is not
    // part of the path, and a console that 404s because somebody's link carried
    // a tracking parameter — or because a browser was asked to reload past its
    // cache — is a console that looks broken for a reason nobody can see.
    let path = path.split_once('?').map_or(path, |(before, _)| before);
    let at = ASSETS.iter().position(|(at, _, _)| *at == path)?;
    let (_, kind, body) = ASSETS.get(at)?;
    let tag = TAGS.get(at)?.as_str();
    // Weak comparison, as RFC 9110 §13.1.2 asks of `If-None-Match`: a `W/` a
    // proxy put in front of the tag does not make it a different tag.
    let held = if_none_match.is_some_and(|listed| {
        listed.split(',').map(str::trim).any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == tag
        })
    });
    let mut answer = if held {
        Answer::text(304, String::new(), kind)
    } else {
        Answer::text(200, (*body).to_owned(), kind)
    };
    answer.tag = Some(tag);
    Some(answer)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::ASSETS;

    /// The page's own text, which the tests below read rather than assume.
    fn page() -> &'static str {
        ASSETS
            .iter()
            .find_map(|(at, _, body)| (*at == "/").then_some(*body))
            .expect("the console serves a root page")
    }

    #[test]
    fn the_roles_the_page_offers_are_the_roles_this_build_has() {
        // The page hard-codes three roles, which is the only way it can offer a
        // list without a statement to ask for one. So the list is pinned to the
        // engine's own `Role::ALL` — the constant that exists, in its own words,
        // "so a listing cannot drift from the set". A fourth role added to the
        // storage crate and not to the page fails here rather than quietly
        // becoming a role nobody can grant from the console.
        let page = page();
        // Scoped to the selects that offer roles, rather than to every option on
        // the page minus a list of exceptions. The exception list was the first
        // shape and it grew by one every time an unrelated dropdown was added,
        // which is a check that quietly loosens as the page fills up.
        for id in ["new-role", "change-role"] {
            let offered = options(page, id);
            for role in tessari_storage::Role::ALL {
                assert!(
                    offered.iter().any(|value| value == role.name()),
                    "{id} offers no way to choose the {} role",
                    role.name()
                );
            }
            // And the other direction: an option the engine has never heard of.
            // The one exception is `other`, the free-text escape the page offers
            // on purpose — a role this build refuses is shown refusing, rather
            // than hidden behind a control that pretends it cannot be asked for.
            for value in &offered {
                assert!(
                    value == "other"
                        || tessari_storage::Role::ALL
                            .iter()
                            .any(|role| role.name() == value),
                    "{id} offers a {value:?} role and this build has no such thing"
                );
            }
        }
    }

    /// The `value` of every `<option>` inside one named `<select>`.
    ///
    /// Panics when the select is not there, which is the point: a renamed
    /// control would otherwise turn its cross-check into a loop over nothing,
    /// and a test that asserts about an empty list passes for the wrong reason.
    fn options(page: &str, id: &str) -> Vec<String> {
        let opening = format!(r#"<select id="{id}">"#);
        let at = page
            .find(&opening)
            .unwrap_or_else(|| panic!("the page has no <select id={id:?}>"));
        let rest = &page[at.saturating_add(opening.len())..];
        let end = rest
            .find("</select>")
            .unwrap_or_else(|| panic!("<select id={id:?}> is never closed"));
        rest[..end]
            .match_indices(r#"<option value=""#)
            .filter_map(|(found, _)| {
                let after = rest.get(found.saturating_add(r#"<option value=""#.len())..end)?;
                after.split('"').next().map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn every_tab_names_a_panel_that_exists_and_every_panel_has_a_tab() {
        // Both directions, because each catches a different half-finished edit:
        // a tab whose panel was never added is a section that shows nothing, and
        // a panel whose tab was never added is a section nobody can reach. The
        // second is the quieter one — the markup is all there, and it is simply
        // invisible.
        //
        // Scanned PER ROLE and not per attribute. `aria-controls` used to be a
        // reliable stand-in for "this is a tab", and it stopped being one the
        // moment a disclosure button acquired one — which is correct ARIA and
        // not something this test gets to forbid. A needle that was a proxy for
        // the subject silently becomes a needle for something else.
        let page = page();
        let named = |role: &str, attribute: &str| -> Vec<String> {
            page.lines()
                .filter(|line| line.contains(role))
                .filter_map(|line| {
                    let at = line.find(attribute)?;
                    let rest = line.get(at.saturating_add(attribute.len())..)?;
                    rest.split('"').next().map(str::to_owned)
                })
                .collect()
        };
        let controls = named(r#"role="tab""#, r#"aria-controls=""#);
        let labelled = named(r#"role="tabpanel""#, r#"aria-labelledby=""#);
        assert!(!controls.is_empty(), "the page has no tabs at all");
        assert_eq!(
            controls.len(),
            labelled.len(),
            "{controls:?} vs {labelled:?}"
        );

        for panel in &controls {
            assert!(
                page.contains(&format!(r#"id="{panel}""#)),
                "a tab controls {panel}, which is not on the page"
            );
        }
        for tab in &labelled {
            assert!(
                page.contains(&format!(r#"id="{tab}""#)),
                "a panel is labelled by {tab}, which is not on the page"
            );
        }
    }

    #[test]
    fn every_asset_the_page_references_is_one_this_module_serves() {
        // The obligation runs this way round on purpose: the page names what it
        // needs, and the table has to satisfy it. Reading the table and then
        // checking the page mentions each entry would pass while the page
        // referenced a fourth file nobody embedded.
        let page = page();
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
