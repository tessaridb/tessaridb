//! The enforcement-point inventory, checked against the code it was derived
//! from.
//!
//! # Why a test and not a paragraph
//!
//! The permission coverage matrix was derived mechanically from the engine's own
//! tables — every route, every frame kind, every `pub fn` on `Db` and `Store`,
//! every command-line source — and each one was then classified as enforced,
//! exempt, or not a data path. That derivation was correct on the day it ran and
//! has no opinion about the next day: **a hand-maintained matrix decays at
//! exactly the rate new endpoints are added**, and nothing about adding a route
//! makes anybody open it.
//!
//! So the tables are counted here. A new HTTP route, a new frame kind, a new
//! `pub fn` on `Db` or `Store` fails this test until somebody has classified it
//! and moved the number — which is a thirty-second edit if the path is exempt
//! and the right conversation if it is not.
//!
//! # What is counted, and what is not
//!
//! Seven tables, carrying **61** of the derivation's 70 paths.
//!
//! # The path added by the vault, and how it was classified
//!
//! `Store::vault` is the sixty-first, and it is the one path here that is not
//! reached by a grant. Classified **enforced, by a second and independent
//! mechanism**: what it hands out is the `OpenVault`, and the key inside it is
//! lent for the duration of one call by `with_master` and cannot be kept. A
//! caller holding every grant the system offers and no passphrase is refused at
//! decryption; a caller holding the passphrase and no grant is refused at the
//! door by the ordinary reach check. Neither passes by the other's route, which
//! is criterion F3 of the vault goal stated as a coverage decision.
//!
//! The classification is recorded here rather than only in the count, because
//! the count is a reminder and the classification is the work. The nine that are
//! not counted, named rather than quietly dropped, because a check that narrows
//! its own scope in silence is worse than one that never ran:
//!
//! - **the object-store fallthrough (6)** — its arms are `Method::` patterns
//!   nested inside the outer match's fallthrough arm, not a table; counting them
//!   would mean matching on indentation, which is a check on formatting;
//! - **`StatementKind` (1 row, 52 kinds)** — already guarded better than this
//!   could: `Needs::of` matches it exhaustively with no catch-all, so a new
//!   statement does not compile until it is classified;
//! - **the pre-match route (1)** and **the declared-consumer apply (1)** — one
//!   of each, with no table to grow.
//!
//! # The second assertion is the one that protects an exemption
//!
//! Twenty-five paths are exempt under **E3**: the embedded facade is a boundary
//! the permission system deliberately does not defend, because a caller holding
//! `&Store` holds every byte in it and no check stands between them. That
//! exemption is sound exactly while the surfaces that *are* on the network do
//! not use the facade to get around their own checks.
//!
//! Today both feed surfaces call the authorized `feed::follow` rather than the
//! raw `Db::poll`. Nothing stopped the next surface from doing otherwise, and
//! nothing would have noticed — the code would compile, the tests would pass, and
//! a subscription would stream records past every grant in the store.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The body of a top-level block, from its opening line to the `}` in column
/// zero that closes it.
///
/// Braces are not counted, deliberately: the doc comments in this tree are full
/// of `{ … }`, and a counter that walked them would be reading prose as code.
/// A top-level `impl` or `enum` closes at column zero, which is a property of
/// the formatter and is therefore checked by `cargo fmt` before it is relied on
/// here.
fn block(text: &str, header: &str) -> String {
    let opening = format!("\n{header} {{\n");
    let at = text
        .find(&opening)
        .unwrap_or_else(|| panic!("`{header}` is no longer where this check reads it"));
    let body = text
        .get(at.saturating_add(opening.len())..)
        .unwrap_or_default();
    let end = body
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`{header}` never closes at column zero"));
    body.get(..end).unwrap_or_default().to_owned()
}

/// Lines whose trimmed form begins with `prefix`.
///
/// A doc line begins with `///` and an attribute with `#`, so neither is
/// counted, which is what lets this read a table without parsing Rust.
fn lines_beginning(text: &str, prefix: &str) -> usize {
    text.lines()
        .filter(|line| line.trim_start().starts_with(prefix))
        .count()
}

/// Public functions declared directly in a block.
fn public_functions(body: &str) -> usize {
    ["pub fn ", "pub const fn ", "pub async fn "]
        .iter()
        .map(|prefix| lines_beginning(body, prefix))
        .sum()
}

/// Public functions of a **module**, which are the ones at column zero.
///
/// The indentation is the distinction and it is not cosmetic: a `pub fn` inside
/// an `impl` is a method on a type the module happens to define, and a caller
/// reaches it only by already holding one of those. `feed.rs` is where this
/// matters — it exports one function and defines a small waiting type with two
/// methods of its own, and counting all three would put a condition variable in
/// the enforcement inventory.
fn module_functions(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("pub fn ") || line.starts_with("pub async fn "))
        .count()
}

/// Variants of an enum body: a four-space indent and a capital letter.
fn variants(body: &str) -> usize {
    body.lines()
        .filter(|line| {
            line.strip_prefix("    ")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
        })
        .count()
}

/// One table of enforcement points: where it lives, and how many it held when
/// the matrix classified them.
struct Table {
    file: &'static str,
    what: &'static str,
    expected: usize,
    count: fn(&str) -> usize,
}

const TABLES: &[Table] = &[
    Table {
        file: "crates/tessaridb/src/lib.rs",
        what: "public methods on `Db` — the embedded facade",
        expected: 16,
        count: |text| public_functions(&block(text, "impl Db")),
    },
    Table {
        file: "crates/tessari-storage/src/store.rs",
        what: "public methods on `Store` — the substrate below the facade",
        // 15 since the vault: `Store::vault` hands out the per-process keyring.
        // Classified **enforced**, and by a different mechanism from every other
        // method here — the keyring is not reached by a grant but by holding a
        // passphrase, which is the point of criterion F3: reach and open are
        // separate, and neither passes by the other's route. What it hands out
        // is the `OpenVault` itself and never the key inside it; `with_master`
        // lends the key for one call and no caller can keep it.
        expected: 15,
        count: |text| public_functions(&block(text, "impl Store")),
    },
    Table {
        file: "crates/tessari-cli/src/arguments.rs",
        what: "what the command line can be asked to do",
        expected: 10,
        count: |text| variants(&block(text, "pub enum Source")),
    },
    Table {
        file: "crates/tessari-wire/src/frame.rs",
        what: "frame kinds the binary protocol accepts",
        expected: 5,
        count: |text| variants(&block(text, "pub(crate) enum Kind")),
    },
    Table {
        file: "crates/tessari-http/src/lib.rs",
        what: "routes matched by method and path",
        expected: 8,
        count: |text| lines_beginning(text, "(Method::"),
    },
    Table {
        file: "crates/tessari-backup/src/lib.rs",
        what: "the backup surface, which takes a store and no identity",
        expected: 6,
        count: module_functions,
    },
    Table {
        file: "crates/tessaridb/src/feed.rs",
        what: "the authorized change feed",
        expected: 1,
        count: module_functions,
    },
];

#[test]
fn every_enforcement_point_table_holds_what_the_coverage_matrix_classified() {
    let mut moved = Vec::new();
    let mut total = 0;
    for table in TABLES {
        let found = (table.count)(&read(table.file));
        total += found;
        if found != table.expected {
            moved.push(format!(
                "{}: {} — {} now, {} when they were classified",
                table.file, table.what, found, table.expected
            ));
        }
    }
    assert!(
        moved.is_empty(),
        "{} enforcement-point table(s) changed size:\n  {}\n\n\
         Every one of these is a place a caller reaches this store. Classify the \
         new path — enforced, or exempt under a named boundary — in the coverage \
         matrix, then move the number here. Do not move the number first: the \
         count is the reminder, and the classification is the work.",
        moved.len(),
        moved.join("\n  "),
    );
    assert_eq!(total, 61, "the counted tables no longer sum to 61");
}

/// Every `.rs` file under a directory.
fn sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        panic!("{} is not readable", directory.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|kind| kind == "rs") {
            found.push(path);
        }
    }
    found
}

/// The raw change feed on the embedded facade, and the authorized call that
/// exists instead of each.
///
/// Not the whole E3 list. `Db::names_in`, `Db::store` and `Db::writable_peer`
/// are on it too and are called from these crates today — legitimately, because
/// none of them has an authorized alternative to reach for: `names_in` resolves
/// ids the caller already holds, `store` is the named gateway the facade exists
/// to expose, and `writable_peer` reports topology. Forbidding those would fail
/// on the current tree, and a rule that has to be suppressed on the day it is
/// written teaches everybody to suppress it.
///
/// These four are different: each has an enforced replacement, and reaching past
/// it is never anything but a mistake.
const RAW_FEED: &[&str] = &[
    ".poll(",
    ".changes_since(",
    ".subscribe(",
    ".committed_tail(",
];

#[test]
fn no_serving_surface_reaches_the_raw_change_feed() {
    let mut reached = Vec::new();
    for crate_name in ["tessari-http", "tessari-wire", "tessari-cli"] {
        let root = repo().join("crates").join(crate_name).join("src");
        for path in sources(&root) {
            let text = fs::read_to_string(&path).unwrap();
            for (number, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for call in RAW_FEED {
                    if line.contains(call) {
                        reached.push(format!("{}:{} {}", path.display(), number + 1, line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        reached.is_empty(),
        "a surface on the network reaches the unfiltered change feed:\n  {}\n\n\
         `feed::follow` is the authorized call and does the same job: it asks \
         whether this session may read, narrows the tables to the ones it was \
         granted, redacts the fields it was not, and re-asks all three on every \
         poll round so a revocation ends the subscription. Reaching past it \
         streams records past every grant in the store, and nothing raises an \
         error while it happens.",
        reached.join("\n  "),
    );
}
