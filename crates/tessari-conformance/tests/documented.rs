//! The documentation ratchet: the specification names everything the engine has.
//!
//! # Why this exists
//!
//! The corpus cannot fall behind the language, because [`form_name`] matches
//! `StatementKind` exhaustively and the build stops until a new form is named,
//! and `FORMS` then fails the coverage test until a case exists. **Nothing
//! played that role for the prose**, so the document was written per goal and a
//! goal covers what that goal touched. Measured on 2026-08-29, the site built
//! from this document was missing twelve of the twenty declarable kinds and
//! nearly half the functions — not stragglers, but whole families: every
//! `time::` accessor, all of `object::`, most of `type::`.
//!
//! A rewrite alone would have fixed that number and then decayed at exactly the
//! rate the last one did. This is the ratchet that stops it: add a kind, a
//! function or a statement form, and the suite fails until the document says
//! what it is.
//!
//! # What counts as documented
//!
//! Deliberately mechanical, and deliberately not "the word appears somewhere":
//!
//! - A **kind** must appear in a `TYPE` declaration, which is where a reader
//!   looking for the set will be.
//! - A **function** must appear as a call, `name(`, so that naming it in a
//!   sentence about something else does not count.
//! - A **form** must appear as a fenced example that *parses to it* — the same
//!   extraction the corpus ratchet uses, so a form cannot be covered by a
//!   heading that happens to spell it.
//!
//! # The allow-list
//!
//! [`UNDOCUMENTED`] holds what is deliberately absent, each with the reason.
//! It is checked in **both directions**: an entry that has since been
//! documented fails too, because an allow-list nobody prunes is how the next
//! gap hides.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;

use tessari_conformance::{FORMS, examples, forms_in, specification};
use tessari_ql::Function;
use tessari_types::FieldKind;

/// What the specification is allowed not to name yet, and why.
///
/// A reason is not a formality: the only defensible entry is syntax the
/// released engine does not yet enforce, because documenting protection that
/// does not exist is worse than the silence.
/// Empty, and that is the state it is supposed to be in. The two entries it
/// held — `GRANT ON REACH` and `REVOKE ON REACH` — were excused while the
/// authority model was committed and unenforced, on the ground that documenting
/// protection the store does not yet keep is worse than the silence. Enforcement
/// shipped and §4 now describes both, so the excuse expired and the entries went
/// with it, which is the direction of this check that usually goes unexercised.
const UNDOCUMENTED: &[(&str, &str)] = &[];

/// Whether `unit` is on the allow-list.
fn excused(unit: &str) -> bool {
    UNDOCUMENTED.iter().any(|(name, _)| *name == unit)
}

/// Every unit the allow-list excuses, for the staleness check.
fn excused_units() -> BTreeSet<&'static str> {
    UNDOCUMENTED.iter().map(|(name, _)| *name).collect()
}

/// Reports the units of `set` that `covered` does not hold, minus the excused.
///
/// Returns the missing and the stale separately: a unit that is both covered
/// and excused is a rotting allow-list entry, and is as much a defect as a gap.
fn reconcile(
    set: &BTreeSet<String>,
    covered: &BTreeSet<String>,
) -> (Vec<String>, Vec<&'static str>) {
    let missing = set
        .iter()
        .filter(|unit| !covered.contains(*unit) && !excused(unit))
        .cloned()
        .collect();
    let stale = excused_units()
        .into_iter()
        .filter(|unit| set.contains(*unit) && covered.contains(*unit))
        .collect();
    (missing, stale)
}

/// Asserts a source set is covered, naming what is not and what no longer needs
/// excusing.
fn assert_covered(what: &str, set: &BTreeSet<String>, covered: &BTreeSet<String>) {
    let (missing, stale) = reconcile(set, covered);
    assert!(
        stale.is_empty(),
        "the allow-list still excuses {} that the specification now documents — \
         remove {stale:?} from UNDOCUMENTED",
        what,
    );
    assert!(
        missing.is_empty(),
        "{} of {} {what} appear nowhere in docs/tessariql.md: {missing:?}\n\
         Document them, or add each to UNDOCUMENTED with the reason it is \
         deliberately absent. The site's reference pages are written from this \
         document, so a gap here becomes a gap on docs.tessaridb.com.",
        missing.len(),
        set.len(),
    );
}

/// Every kind that can be written in a `TYPE` clause.
///
/// `Literal` carries its members rather than naming a value type, so it has no
/// fixed spelling; it is represented by the form a reader would write.
fn declarable_kinds() -> BTreeSet<String> {
    let mut kinds: BTreeSet<String> = FieldKind::all()
        .iter()
        .map(|kind| kind.name().into_owned())
        .collect();
    kinds.insert("literal union".to_owned());
    kinds
}

#[test]
fn every_declarable_kind_is_named_in_a_type_clause() {
    let text = specification();
    let mut covered = BTreeSet::new();
    for kind in declarable_kinds() {
        let named = if kind == "literal union" {
            // A union has no single spelling, so what is checked is that the
            // document shows one being declared at all.
            text.contains("TYPE '")
        } else {
            // `TYPE option<record>` and `TYPE array<int>` both name their inner
            // kind, so the boundary is any non-identifier character.
            declared(&text, &kind)
        };
        if named {
            covered.insert(kind);
        }
    }
    assert_covered("declarable kinds", &declarable_kinds(), &covered);
}

/// Whether the document declares `kind`, in a `TYPE` clause rather than in
/// prose that happens to use the word.
fn declared(text: &str, kind: &str) -> bool {
    text.match_indices("TYPE ").any(|(at, _)| {
        let Some(clause) = text.get(at..) else {
            return false;
        };
        let clause = clause.lines().next().unwrap_or_default();
        clause.match_indices(kind).any(|(start, _)| {
            let before = clause[..start].chars().next_back();
            let after = clause[start.saturating_add(kind.len())..].chars().next();
            let boundary = |character: Option<char>| {
                character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            };
            boundary(before) && boundary(after)
        })
    })
}

fn function_spellings() -> BTreeSet<String> {
    Function::ALL
        .iter()
        .map(|function| function.spelling().to_owned())
        .collect()
}

#[test]
fn every_function_is_shown_being_called() {
    let text = specification();
    let all = function_spellings();
    let covered = all
        .iter()
        .filter(|spelling| text.contains(&format!("{spelling}(")))
        .cloned()
        .collect();
    assert_covered("functions", &all, &covered);
}

fn statement_forms() -> BTreeSet<String> {
    FORMS.iter().map(|form| (*form).to_owned()).collect()
}

#[test]
fn every_statement_form_has_an_example_that_parses_to_it() {
    // The same extraction the corpus ratchet uses, pointed at the document: a
    // form is covered when an example in the prose *is* that statement, never
    // when a heading merely spells it.
    let text = specification();
    let mut covered = BTreeSet::new();
    for (_, statement) in examples(&text) {
        if let Ok(script) = tessari_ql::parse(&statement) {
            covered.extend(forms_in(&script).into_iter().map(ToOwned::to_owned));
        }
    }
    assert_covered("statement forms", &statement_forms(), &covered);
}

/// Whether a call is the document naming the *shape* of a call rather than one.
fn spelling_is_the_placeholder(group: &str, name: &str) -> bool {
    group == "group" && name == "name"
}

#[test]
fn the_specification_calls_no_function_the_engine_does_not_have() {
    // The inverse of the ratchet, and the more embarrassing failure of the two:
    // a gap leaves a reader looking elsewhere, while an invented function leaves
    // them writing it and getting `NoSuchFunction`. The site built from this
    // document carried two — `string::length` and a bare `len` — for as long as
    // nothing checked this direction.
    let text = specification();
    let mut invented = BTreeSet::new();
    for (at, _) in text.match_indices("::") {
        let Some(before) = text.get(..at) else {
            continue;
        };
        let group: String = before
            .chars()
            .rev()
            .take_while(|character| character.is_ascii_lowercase())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let Some(after) = text.get(at.saturating_add(2)..) else {
            continue;
        };
        let name: String = after
            .chars()
            .take_while(|character| character.is_ascii_lowercase() || *character == '_')
            .collect();
        if group.is_empty() || name.is_empty() {
            continue;
        }
        // The group must be a whole word. Without this, `Outcome::plan()` — a
        // Rust name the document legitimately mentions — arrives here as a call
        // to `utcome::plan`, because the scan stops at the capital.
        let whole = before
            .get(..before.len().saturating_sub(group.len()))
            .and_then(|earlier| earlier.chars().next_back())
            .is_none_or(|character| !character.is_alphanumeric() && character != '_');
        // `group::name(…)` is how the document names the shape itself, one
        // paragraph before listing the real ones.
        if !whole || spelling_is_the_placeholder(&group, &name) {
            continue;
        }
        // Only a call counts. `the `string` group` is prose about a group, and
        // reading it as a claim would make the check unusable.
        let called = after
            .get(name.len()..)
            .is_some_and(|rest| rest.starts_with('('));
        let spelling = format!("{group}::{name}");
        if called && Function::ALL.iter().all(|f| f.spelling() != spelling) {
            invented.insert(spelling);
        }
    }
    assert!(
        invented.is_empty(),
        "docs/tessariql.md shows {invented:?} being called, and the engine has \
         no such function — a reader following the document gets NoSuchFunction"
    );
}

#[test]
fn the_units_the_documentation_must_cover_can_be_printed() {
    // The documentation site is a separate repository and deliberately depends
    // on this one by no mechanism — it reaches the database over the wire like
    // any other caller. So it cannot import these sets, and its own ratchet
    // reads a checked-in manifest instead. This test prints that manifest:
    //
    //     cargo test -p tessari-conformance --test documented \
    //         the_units_the_documentation_must_cover_can_be_printed -- --nocapture
    //
    // Regenerating it by hand is the failure this whole file exists to stop, so
    // the manifest is produced here and copied, never typed.
    let mut lines = vec![
        "# Generated. Do not edit by hand.".to_owned(),
        "# Produced by tessari-conformance's `documented` suite from FieldKind,".to_owned(),
        "# Function::ALL and FORMS. Regenerate when that suite tells you to.".to_owned(),
    ];
    for (heading, units) in [
        ("kind", declarable_kinds()),
        ("function", function_spellings()),
        ("form", statement_forms()),
    ] {
        for unit in units {
            let excused = if excused(&unit) { "\texcused" } else { "" };
            lines.push(format!("{heading}\t{unit}{excused}"));
        }
    }
    println!("{}", lines.join("\n"));
    assert!(lines.len() > 100, "the manifest is suspiciously short");
}

#[test]
fn the_allow_list_gives_a_reason_for_every_entry() {
    for (unit, reason) in UNDOCUMENTED {
        assert!(
            reason.len() > 30,
            "{unit} is excused without a reason worth reading: {reason:?}"
        );
    }
}
