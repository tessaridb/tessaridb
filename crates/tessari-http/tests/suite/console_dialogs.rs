//! What a visitor meets before signing in, and where an action on an account lives.
//!
//! The gate is drawn by the page, but the control is the node: a store that has
//! declared a user refuses an unsigned script with `401`, and one that has not
//! answers it. The first test pins both halves, because a gate that asked a
//! question the node does not answer that way would either lock everybody out of
//! an open store or show a closed one to anybody.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

#[cfg(feature = "console")]
use crate::console::{get, node, panel_sources, script};

/// The page as a browser receives it.
#[cfg(feature = "console")]
fn page() -> String {
    let (_node, address) = node();
    let (status, _, page) = get(&address, "/");
    assert_eq!(status, 200, "the console's page is not served");
    page
}

/// Where the element carrying `id` starts, or a failure naming it.
#[cfg(feature = "console")]
fn position(page: &str, id: &str) -> usize {
    page.find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("the page declares no #{id}"))
}

#[cfg(feature = "console")]
#[test]
fn the_gate_asks_the_question_the_node_already_answers() {
    let (_node, address) = node();
    let (open, body) = script(&address, "RETURN 1;");
    assert_eq!(
        open, 200,
        "a store with no user refused the probe, so the gate would lock everybody \
         out of it: {body}"
    );
    let (defined, body) = script(
        &address,
        "DEFINE USER owner ROLE owner PASSWORD 'g071-test-only';",
    );
    assert_eq!(defined, 200, "the owner could not be declared: {body}");
    let (closed, body) = script(&address, "RETURN 1;");
    assert_eq!(
        closed, 401,
        "a store with a user answered an unsigned script, so the gate would be the \
         only thing standing in front of its data: {body}"
    );

    let sources = panel_sources();
    let gate = sources
        .iter()
        .find(|(name, _)| name == "gate.ts")
        .map(|(_, text)| text.as_str())
        .expect("the panel has no gate module");
    assert!(
        gate.contains("\"RETURN 1;\"") && gate.contains("401") && gate.contains("ask("),
        "the gate does not put the probe this test pins through the console's one \
         path to the node"
    );
}

#[cfg(feature = "console")]
#[test]
fn a_visitor_who_is_not_signed_in_meets_the_sign_in_page_first() {
    let page = page();
    let gate = position(&page, "gate");
    let shell = position(&page, "shell");
    assert!(
        gate < shell,
        "the sign-in page is not drawn before the console"
    );
    let card = &page[gate..shell];
    for id in ["user", "password", "sign-in", "identity-status"] {
        assert!(
            card.contains(&format!("id=\"{id}\"")),
            "#{id} is not on the sign-in page, so signing in still happens inside \
             the console it is meant to guard"
        );
    }
    let opened = page[..shell].rfind('<').expect("an opening tag");
    let tag = &page[opened..shell.saturating_add(page[shell..].find('>').unwrap_or(0))];
    assert!(
        tag.contains(" hidden"),
        "the console is drawn before the node has said whether it needs a sign-in"
    );
}

#[cfg(feature = "console")]
#[test]
fn every_action_on_an_account_is_its_own_dialog() {
    let page = page();
    for id in [
        "define",
        "change",
        "remove",
        "grant-apply",
        "mine",
        "form-cluster",
        "revoke-apply",
        "join-apply",
        "failover-apply",
    ] {
        let at = position(&page, id);
        let opened = page[..at].rfind("<dialog").unwrap_or_else(|| {
            panic!("#{id} stands on a screen instead of in a dialog of its own")
        });
        let closed = page[opened..]
            .find("</dialog>")
            .map(|end| opened.saturating_add(end))
            .expect("the dialog closes");
        assert!(
            at < closed,
            "#{id} stands on a screen instead of in a dialog of its own"
        );
    }

    let mut dialogs = 0_usize;
    for (opened, _) in page.match_indices("<dialog") {
        dialogs = dialogs.saturating_add(1);
        let tag_end = opened.saturating_add(page[opened..].find('>').expect("a tag"));
        let tag = &page[opened..tag_end];
        let label = tag
            .split("aria-labelledby=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("a dialog carries no label: {tag}"));
        assert!(
            page.contains(&format!("id=\"{label}\"")),
            "a dialog is labelled by #{label}, which the page does not declare"
        );
        let body_end = opened.saturating_add(page[opened..].find("</dialog>").expect("closes"));
        assert!(
            page[opened..body_end].contains("data-close"),
            "the dialog labelled #{label} has no control that closes it"
        );
    }
    assert!(
        dialogs >= 10,
        "only {dialogs} dialog(s) on the page — the account actions are not each \
         their own"
    );
}

#[cfg(feature = "console")]
#[test]
fn no_screen_reads_behind_the_sign_in_page() {
    // Measured in W2's browser pass: a reload on `#cluster` while signed out sent
    // two statements before the gate answered, both refused. The screens read on
    // arrival, so arrival has to wait for the gate — and has to stop again
    // whenever the sign-in page comes back.
    let sources = panel_sources();
    let source = |wanted: &str| -> String {
        sources
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("{wanted} is not among the panel's sources"))
    };
    let tabs = source("tabs.ts");
    assert!(
        tabs.contains("let held = true;"),
        "the screens may read before the gate has answered"
    );
    assert!(
        tabs.matches("if (held)").count() >= 2 && tabs.contains("!held &&"),
        "an arrival path reads without asking whether the screens are held"
    );
    let gate = source("gate.ts");
    let shown = gate
        .split("function showGate(")
        .nth(1)
        .and_then(|body| body.split("\n}").next())
        .expect("the gate has a showGate");
    assert!(
        shown.contains("hold(true)"),
        "showing the sign-in page does not hold the screens behind it"
    );
    let console = gate
        .split("function showConsole(")
        .nth(1)
        .and_then(|body| body.split("\n}").next())
        .expect("the gate has a showConsole");
    let released = console
        .find("hold(false)")
        .expect("the console never lets go");
    let read = console
        .find("hereAgain()")
        .expect("the console never reads");
    assert!(
        released < read,
        "the console reads the screen before letting go of it, so the read is held"
    );
}
