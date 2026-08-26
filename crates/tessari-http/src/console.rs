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
/// `include_str!` rather than `include_bytes!` because all five are text, and
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
    (
        "/sections.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/sections.js"),
    ),
    // Named by the page, so a browser asks for this instead of `/favicon.ico`
    // and the 404 that would otherwise sit in every operator's console.
    (
        "/favicon.svg",
        "image/svg+xml",
        include_str!("../assets/favicon.svg"),
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
    // Split on `?` for the same reason `/backup` does: a query string is not
    // part of the path, and a console that 404s because somebody's link carried
    // a tracking parameter — or because a browser was asked to reload past its
    // cache — is a console that looks broken for a reason nobody can see.
    let path = path.split_once('?').map_or(path, |(before, _)| before);
    ASSETS.iter().find_map(|(at, kind, body)| {
        (*at == path).then(|| Answer::text(200, (*body).to_owned(), kind))
    })
}

#[cfg(test)]
mod tests {
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
        for role in tessari_storage::Role::ALL {
            assert!(
                page.contains(&format!(r#"<option value="{}""#, role.name())),
                "the page offers no way to choose the {} role",
                role.name()
            );
        }
        // And the other direction: an option the engine has never heard of. The
        // deliberate exception is `other`, which is the free-text escape the
        // page offers on purpose — a role this build refuses is shown refusing,
        // rather than being hidden behind a control that pretends it cannot.
        let offered: Vec<&str> = page
            .match_indices(r#"<option value=""#)
            .filter_map(|(at, _)| {
                let rest = page.get(at.saturating_add(r#"<option value=""#.len())..)?;
                rest.split('"').next()
            })
            .filter(|value| *value != "other" && *value != "space" && *value != "node")
            .collect();
        for value in offered {
            assert!(
                tessari_storage::Role::ALL
                    .iter()
                    .any(|role| role.name() == value),
                "the page offers a {value:?} role and this build has no such thing"
            );
        }
    }

    #[test]
    fn every_tab_names_a_panel_that_exists_and_every_panel_has_a_tab() {
        // Both directions, because each catches a different half-finished edit:
        // a tab whose panel was never added is a section that shows nothing, and
        // a panel whose tab was never added is a section nobody can reach. The
        // second is the quieter one — the markup is all there, and it is simply
        // invisible.
        let page = page();
        let named = |attribute: &str| -> Vec<String> {
            page.match_indices(attribute)
                .filter_map(|(at, _)| {
                    let rest = page.get(at.saturating_add(attribute.len())..)?;
                    rest.split('"').next().map(str::to_owned)
                })
                .collect()
        };
        let controls = named(r#"aria-controls=""#);
        let labelled = named(r#"aria-labelledby=""#);
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
