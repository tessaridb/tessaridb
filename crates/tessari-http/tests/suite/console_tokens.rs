//! Whether the console's token layer says what the design brief decided.
//!
//! These are not style opinions asserted as tests. Each one checks a number the
//! brief fixed as a dial — motion 80-120ms, rows 32-40px, chroma ≤0.15 — against
//! the stylesheet this process actually serves. A screenshot cannot see any of
//! them: a 300ms transition looks like a 100ms one in a still image, a row at
//! 26px looks deliberate, and proportional digits look like digits.
//!
//! What no test here does is measure LAYOUT. There is no browser in this suite,
//! so the row-height assertion is arithmetic over declared tokens and says so in
//! its own failure message. It proves the stylesheet asks for 34px. It does not
//! prove a browser draws 34px.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use crate::console::{get, node};

/// The stylesheet as a browser receives it — not as the repository holds it.
#[cfg(feature = "console")]
fn stylesheet() -> String {
    let (_node, address) = node();
    let (status, _, css) = get(&address, "/console.css");
    assert_eq!(status, 200, "the console's stylesheet is not served");
    assert!(
        css.contains(":root"),
        "the stylesheet came back without a token block, so every assertion \
         below would pass by finding nothing"
    );
    css
}

/// Every `{ … }` body in the sheet, so a declaration can be asked what rule it
/// is in. Crude by design: the sheet has no nesting, and a parser that handled
/// nesting would be a second thing to maintain.
#[cfg(feature = "console")]
fn rule_bodies(css: &str) -> Vec<&str> {
    css.split('}')
        .filter_map(|piece| piece.split_once('{').map(|(_, body)| body))
        .collect()
}

/// Every number written immediately before `unit`, in source order.
#[cfg(feature = "console")]
fn values_with_unit(css: &str, unit: &str) -> Vec<f64> {
    let mut found = Vec::new();
    for (at, _) in css.match_indices(unit) {
        let head = &css[..at];
        let start = head
            .rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map_or(0, |i| i.saturating_add(1));
        if let Ok(value) = head[start..].parse::<f64>() {
            found.push(value);
        }
    }
    found
}

#[cfg(feature = "console")]
#[test]
fn every_transition_duration_is_inside_the_dial() {
    let css = stylesheet();

    assert!(
        css.contains("transition:"),
        "the stylesheet declares no transition at all. The brief's motion dial \
         is 20, not 0 — state feedback is the one thing it buys, and a console \
         whose every state change snaps is the absence this was meant to close"
    );

    let durations = values_with_unit(&css, "ms");
    assert!(
        !durations.is_empty(),
        "no duration was extracted, so this test stopped asking its question"
    );
    let outside: Vec<String> = durations
        .iter()
        .filter(|ms| !(80.0..=120.0).contains(*ms))
        .map(|ms| format!("{ms}ms"))
        .collect();
    assert!(
        outside.is_empty(),
        "MOTION_INTENSITY 20 fixes UI transitions at 80-120ms. Outside the \
         band: {}. A slower one is not a gentler console, it is a console that \
         feels like it is thinking.",
        outside.join(", ")
    );
}

#[cfg(feature = "console")]
#[test]
fn reduced_motion_zeroes_every_transition() {
    let css = stylesheet();
    let at = css
        .find("@media (prefers-reduced-motion: reduce)")
        .expect("the stylesheet honours no reduced-motion preference (brief V15)");
    let block = &css[at..];
    for property in ["transition-duration: 0s", "animation-duration: 0s"] {
        assert!(
            block.contains(property),
            "the reduced-motion block does not carry `{property}`, so a reader \
             who asked for stillness gets motion anyway"
        );
    }
}

#[cfg(feature = "console")]
#[test]
fn every_value_position_uses_tabular_numerals() {
    // The pairing is asserted rather than a list of selectors, because a list is
    // a second opinion about where values live and it agrees with the sheet only
    // until somebody adds a mono surface without it.
    let css = stylesheet();
    let mono: Vec<&str> = rule_bodies(&css)
        .into_iter()
        .filter(|body| body.contains("var(--mono)"))
        .collect();

    assert!(
        !mono.is_empty(),
        "no rule uses the mono stack, so this test stopped asking its question"
    );
    let bare: Vec<&str> = mono
        .iter()
        .filter(|body| !body.contains("font-variant-numeric: tabular-nums"))
        .copied()
        .collect();
    assert!(
        bare.is_empty(),
        "a value is drawn in the mono stack without tabular figures, so digits \
         change width as they change and a column of numbers stops lining up. \
         Rules missing it:\n  {}",
        bare.join("\n  ")
    );
}

#[cfg(feature = "console")]
#[test]
fn no_accent_token_is_more_saturated_than_the_dial_allows() {
    let css = stylesheet();
    let mut over = Vec::new();
    let mut seen = 0_usize;
    for line in css.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !name.starts_with("--violet-") {
            continue;
        }
        let Some(at) = value.find("oklch(") else {
            continue;
        };
        let rest = &value[at.saturating_add(6)..];
        let Some(end) = rest.find(')') else { continue };
        let inside = &rest[..end];
        let mut parts = inside.split_whitespace();
        let (Some(_lightness), Some(chroma)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(chroma) = chroma.parse::<f64>() else {
            continue;
        };
        seen = seen.saturating_add(1);
        if chroma > 0.15 {
            over.push(format!("{name}: oklch({inside})"));
        }
    }
    assert!(
        seen > 0,
        "no `--violet-*` token was parsed, so this test stopped asking its \
         question and the accent ceiling is now unguarded"
    );
    over.dedup();
    assert!(
        over.is_empty(),
        "the brief caps the ACCENT's chroma at 0.15 and these exceed it: {}. The \
         ceiling belongs to the accent alone — green, amber and red are semantic \
         chips the brief marks `no change`, and desaturating the danger colour to \
         satisfy an accent rule makes the one colour that must be unmistakable \
         less so.",
        over.join(", ")
    );
}

#[cfg(feature = "console")]
#[test]
fn a_table_row_computes_into_the_density_band() {
    // ARITHMETIC OVER DECLARED TOKENS, NOT LAYOUT. No browser runs here, so this
    // proves the stylesheet asks for a row in the band — not that one is drawn.
    // Stated in the failure message too, because an assertion that overstates
    // its reach is the exact defect this goal exists to close.
    let css = stylesheet();

    let token = |name: &str| -> f64 {
        let at = css
            .find(&format!("{name}:"))
            .unwrap_or_else(|| panic!("no token {name}"));
        let rest = &css[at..];
        let end = rest
            .find(';')
            .unwrap_or_else(|| panic!("token {name} is unterminated"));
        rest[..end]
            .split_whitespace()
            .last()
            .and_then(|value| value.trim_end_matches("rem").parse::<f64>().ok())
            .unwrap_or_else(|| panic!("token {name} is not a rem value"))
    };

    // The vertical padding is read out of the rule rather than assumed, so a
    // change to the rule is a change to this test's input.
    let cells = css
        .split_once("th,\ntd {")
        .map(|(_, body)| body)
        .expect("the table cell rule is not where this test looks for it");
    let padding = cells
        .split_once("padding: var(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(name, _)| name.to_owned())
        .expect("the cell rule declares no padding");

    let root_px = 16.0_f64;
    let text = token("--t-12") * root_px;
    let line = text * 1.5;
    let row = line + token(&padding) * root_px * 2.0;

    assert!(
        (32.0..=40.0).contains(&row),
        "VISUAL_DENSITY 70 puts table rows in 32-40px and the declared tokens \
         compute to {row}px ({text}px text at line-height 1.5, plus var({padding}) \
         above and below). This is arithmetic over the stylesheet, not a measured \
         layout — it says what the sheet asks for, not what a browser draws."
    );
}
