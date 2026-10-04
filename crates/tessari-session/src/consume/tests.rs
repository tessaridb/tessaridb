use tessari_ql::{Expr, ExprKind, FieldPath, Name, Projected, Span};
use tessari_types::Path;

use super::reach_past;
use crate::evaluate::Shaped;

/// A projection that writes out these values and no star.
fn listing(values: Vec<Projected>) -> Shaped {
    Shaped {
        everything: false,
        omit: Vec::new(),
        values,
    }
}

/// A projection that stars, leaving these routes out.
fn starring(omit: Vec<FieldPath>) -> Shaped {
    Shaped {
        everything: true,
        omit,
        values: Vec::new(),
    }
}

fn route(field: &str) -> FieldPath {
    FieldPath {
        path: Path::field(field),
        span: somewhere(),
    }
}

fn somewhere() -> Span {
    Span::new(0, 1)
}

fn reads(field: &str) -> Expr {
    Expr {
        kind: ExprKind::Path(FieldPath {
            path: Path::field(field),
            span: somewhere(),
        }),
        span: somewhere(),
    }
}

fn offers(name: &str, from: &str) -> Projected {
    Projected {
        value: reads(from),
        name: Name {
            text: name.to_owned(),
            span: somewhere(),
        },
    }
}

#[test]
fn a_key_naming_only_what_the_projection_offers_needs_no_overlay() {
    // The common statement, and the one that must keep its current cost:
    // it orders by a name the answer already carries.
    assert!(!reach_past(
        Some(&listing(vec![offers("name", "name")])),
        &[reads("name")]
    ));
}

#[test]
fn a_key_naming_a_field_the_projection_dropped_needs_the_overlay() {
    assert!(reach_past(
        Some(&listing(vec![offers("name", "name")])),
        &[reads("shape")]
    ));
}

#[test]
fn an_alias_counts_as_offered_under_the_name_it_answers_by() {
    // `SELECT address.city AS home … ORDER BY home` — the key names `home`,
    // which the projection offers, so no overlay and no change of behaviour.
    assert!(!reach_past(
        Some(&listing(vec![offers("home", "address")])),
        &[reads("home")]
    ));
    // And the route it came from is *not* offered, so ordering by that
    // instead does need the source.
    assert!(reach_past(
        Some(&listing(vec![offers("home", "address")])),
        &[reads("address")]
    ));
}

#[test]
fn nothing_is_out_of_reach_when_there_is_no_projection() {
    assert!(!reach_past(None, &[reads("anything")]));
}

#[test]
fn one_key_out_of_several_is_enough_to_need_the_overlay() {
    assert!(reach_past(
        Some(&listing(vec![offers("name", "name")])),
        &[reads("name"), reads("shape")]
    ));
}

#[test]
fn a_star_offers_every_name_the_record_has() {
    // Nothing was dropped, so nothing is out of reach — the same answer the
    // no-projection case gives, for the same reason said differently.
    assert!(!reach_past(
        Some(&starring(Vec::new())),
        &[reads("anything")]
    ));
}

#[test]
fn a_star_that_omits_needs_the_overlay_whatever_the_key_names() {
    // Decided without asking which names the key reads, because a route
    // like `address.postcode` leaves the root `address` offered while
    // removing what a key naming it would read — so a root-level check
    // would answer `false` and reintroduce Q-143 through the new clause.
    assert!(reach_past(
        Some(&starring(vec![route("age")])),
        &[reads("name")]
    ));
}
