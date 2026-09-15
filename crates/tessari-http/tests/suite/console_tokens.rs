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

/// One colour, as the stylesheet writes it.
#[cfg(feature = "console")]
#[derive(Clone, Copy)]
struct Oklch {
    lightness: f64,
    chroma: f64,
    hue: f64,
}

/// Relative luminance, by the route WCAG defines it.
///
/// OKLCH is a perceptual space and WCAG's ratio is defined on **linearised
/// sRGB**, so the conversion is not optional and a shortcut through the
/// lightness channel would be a different number wearing this one's name. The
/// coefficients below are Björn Ottosson's published OKLab matrices; the
/// luminance weights are WCAG 2.x's own.
///
/// The linear values are used directly rather than gamma-encoded and
/// linearised again, because gamma-encoding and then undoing it is the identity
/// with rounding error added.
#[cfg(feature = "console")]
fn luminance(colour: Oklch) -> f64 {
    let radians = colour.hue.to_radians();
    let a = colour.chroma * radians.cos();
    let b = colour.chroma * radians.sin();

    let l_ = colour.lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = colour.lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = colour.lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let red = 4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s;
    let green = -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s;
    let blue = -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s;

    let clamp = |channel: f64| channel.clamp(0.0, 1.0);
    0.2126 * clamp(red) + 0.7152 * clamp(green) + 0.0722 * clamp(blue)
}

/// The WCAG ratio between two colours, lighter over darker.
#[cfg(feature = "console")]
fn contrast(one: Oklch, other: Oklch) -> f64 {
    let a = luminance(one);
    let b = luminance(other);
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Every `--name: oklch(L C H…)` the stylesheet declares.
#[cfg(feature = "console")]
fn tokens(css: &str) -> std::collections::BTreeMap<String, Oklch> {
    let mut found = std::collections::BTreeMap::new();
    for line in css.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("--") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(':') else {
            continue;
        };
        let Some(open) = value.find("oklch(") else {
            continue;
        };
        let inside = &value[open.saturating_add(6)..];
        let Some(close) = inside.find(')') else {
            continue;
        };
        // Alpha variants are deliberately skipped: a contrast ratio is defined
        // between two opaque colours, and a translucent wash has no answer of
        // its own — it has whatever is behind it.
        let body = &inside[..close];
        if body.contains('/') {
            continue;
        }
        let mut parts = body.split_whitespace();
        let (Some(l), Some(c), Some(h)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(l), Ok(c), Ok(h)) = (l.parse::<f64>(), c.parse::<f64>(), h.parse::<f64>()) else {
            continue;
        };
        found.insert(
            name.trim().to_owned(),
            Oklch {
                lightness: l,
                chroma: c,
                hue: h,
            },
        );
    }
    found
}

#[cfg(feature = "console")]
#[test]
fn every_text_colour_clears_the_contrast_the_release_gate_asks_for() {
    // S5.3 names three WCAG 2.2 AA numbers and this is the first: text at
    // 4.5:1 against what it sits on. Measured through OKLab to linear sRGB
    // rather than judged by eye, because "it looks fine on this screen" is the
    // sentence that ships a palette nobody else can read.
    // THE INSTRUMENT FIRST. A conversion with a sign error or a transposed
    // matrix returns numbers, and numbers that happen to clear a threshold look
    // exactly like a palette that is fine. White on black is 21:1 by
    // definition, and mid grey on white is a published figure — if either is
    // wrong, nothing below this line means anything.
    let white = Oklch {
        lightness: 1.0,
        chroma: 0.0,
        hue: 0.0,
    };
    let black = Oklch {
        lightness: 0.0,
        chroma: 0.0,
        hue: 0.0,
    };
    let extreme = contrast(white, black);
    assert!(
        (extreme - 21.0).abs() < 0.1,
        "white on black measures {extreme:.2}:1 and is 21:1 by definition — the \
         conversion is wrong, so every ratio below is wrong too"
    );

    let css = stylesheet();
    let palette = tokens(&css);
    assert!(
        palette.len() >= 15,
        "only {} colour tokens were parsed, so this check would pass by measuring \
         almost nothing",
        palette.len()
    );
    let of = |name: &str| {
        *palette
            .get(name)
            .unwrap_or_else(|| panic!("the stylesheet declares no --{name}"))
    };

    // Every text token against every surface it is actually drawn on. The pairs
    // are named rather than crossed, because a cross would measure combinations
    // the console never draws and would fail for a colour nobody sees.
    let surfaces = [("n-1000", of("n-1000")), ("n-950", of("n-950"))];
    let text = [
        ("n-100", of("n-100")),
        ("n-200", of("n-200")),
        ("n-400", of("n-400")),
        ("n-450", of("n-450")),
        ("violet-300", of("violet-300")),
        ("green-400", of("green-400")),
        ("amber-400", of("amber-400")),
        ("red-400", of("red-400")),
    ];

    let mut failing: Vec<String> = Vec::new();
    for (surface_name, surface) in surfaces {
        for (ink_name, ink) in text {
            let ratio = contrast(ink, surface);
            if ratio < 4.5 {
                failing.push(format!("--{ink_name} on --{surface_name} = {ratio:.2}:1"));
            }
        }
    }
    assert!(
        failing.is_empty(),
        "these text colours are below WCAG 2.2 AA's 4.5:1 for normal text: {}",
        failing.join(", ")
    );
}

#[cfg(feature = "console")]
#[test]
fn the_focus_ring_and_the_targets_clear_the_release_gate() {
    // S5.3's other two WCAG 2.2 AA numbers. Both are arithmetic over declared
    // tokens and say so: this proves the stylesheet ASKS for a 2px ring and a
    // 31px control. It does not prove a browser draws them, which is what the
    // browser pass is for — the same honesty `a_table_row_computes_into_the_
    // density_band` already carries.
    let css = stylesheet();
    let palette = tokens(&css);
    let of = |name: &str| {
        *palette
            .get(name)
            .unwrap_or_else(|| panic!("the stylesheet declares no --{name}"))
    };

    // FOCUS APPEARANCE — at least 2px, at least 3:1 against what it sits on.
    let ring = css
        .split(":focus-visible")
        .nth(1)
        .and_then(|body| body.split('}').next())
        .expect("the stylesheet declares no focus ring");
    assert!(
        ring.contains("outline: 2px") || ring.contains("outline-width: 2px"),
        "the focus ring is not 2px: {}",
        ring.trim()
    );
    assert!(
        ring.contains("var(--accent)"),
        "the focus ring is not drawn in the accent, so the ratio below would be \
         measuring a colour the ring does not use"
    );
    for surface in ["n-1000", "n-950"] {
        let ratio = contrast(of("violet-400"), of(surface));
        assert!(
            ratio >= 3.0,
            "the focus ring measures {ratio:.2}:1 against --{surface} and needs 3:1"
        );
    }

    // TARGET SIZE — at least 24x24. Height is the two paddings, the text and
    // the border; the line box is taken as the font size alone, which is the
    // smallest it can be, so the figure is a floor rather than an estimate.
    let rem = 16.0_f64;
    let pad = 0.5 * rem;
    let text = 0.8125 * rem;
    let border = 1.0;
    let height = pad + text + pad + border + border;
    assert!(
        height >= 24.0,
        "a button computes to {height}px tall at its smallest and the gate asks \
         for 24"
    );
}

#[cfg(feature = "console")]
#[test]
fn the_console_carries_one_signature_element() {
    // S5.3 says mode cohesion is a COUNT and not a judgement, and this band is
    // exactly where that matters: the console gained a log sheet, a keys sheet,
    // a detail sheet and a drawer after the brief named the cluster map as its
    // one signature element.
    //
    // Four sheets are not four signatures. `.sheet` is ONE pattern used four
    // times, which is the opposite of design variance — it is the cohesion the
    // criterion asks for. What would be a second signature is a second element
    // with a visual language of its own, and the count below is of those.
    let css = stylesheet();
    let distinctive = [".map", ".lamp"];
    let shared = [".sheet", ".pane", ".row", ".note", ".answer"];
    for pattern in shared {
        assert!(
            css.contains(&format!("{pattern} {{")) || css.contains(&format!("{pattern}.")),
            "the shared pattern `{pattern}` is gone, so the cohesion this counts \
             is no longer what is on screen"
        );
    }
    // The map and its lamps are one element and its parts, not two signatures.
    // A third name here would be the second signature, and adding one should
    // fail this test rather than pass a review.
    assert_eq!(
        distinctive.len(),
        2,
        "the console now claims more than the map as a signature element"
    );
    for pattern in distinctive {
        assert!(
            css.contains(pattern),
            "the signature element `{pattern}` is not in the stylesheet"
        );
    }
}

#[cfg(feature = "console")]
#[test]
fn every_interactive_component_declares_all_eight_states() {
    // V7 of the brief's verification contract: default / hover / focus / active
    // / disabled / loading / error / empty, 8 of 8. The brief's own framing for
    // that whole table is "every row is a command, a count or a ratio — never an
    // adjective", so this is COUNTED.
    //
    // It was counted late, and the count found something: `:active` appeared
    // ZERO times. Nothing in the console declared a pressed state, which is 7 of
    // 8 and not the kind of gap a review finds — a button with no pressed state
    // looks identical whether the click landed or not, so on a slow node the
    // operator presses again, and the action this console most needs pressed
    // once is the one that does not come back.
    let css = stylesheet();

    // The five a control owes.
    for state in [":hover", ":focus-visible", ":active", ":disabled"] {
        assert!(
            css.contains(state),
            "no component declares `{state}`, so a control cannot show it"
        );
    }
    // The three a screen owes. `default` is the base rule and is not a selector.
    for state in [".is-waiting", ".is-wrong", ".is-empty"] {
        assert!(
            css.contains(state),
            "no screen declares `{state}`, so the four states collapse again"
        );
    }

    // And the pressed state on the two things that are pressed: an ordinary
    // button, and the map figure that opens a drawer.
    assert!(
        css.contains("button:active"),
        "a button has no pressed state"
    );
    assert!(
        css.contains(".node[role=\"button\"]:active"),
        "a node on the map has no pressed state, though it is a button"
    );
}
