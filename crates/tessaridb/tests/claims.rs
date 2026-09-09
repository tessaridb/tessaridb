//! The numbers the repository states about itself, checked against the
//! repository.
//!
//! # Why this is a test and not a habit
//!
//! The claims audit has been run twice by hand, eight waves apart, and found
//! stale statements **both** times — a conformance count 24 cases behind, a
//! crate count off by one, a timing citing a file that records different
//! numbers, and a paragraph describing an index as unread a whole release after
//! it was read. Between the two audits those statements simply stood.
//!
//! Nothing triggered either run. A README goes stale in the quietest way there
//! is: the code changes, the sentence does not, and no build has an opinion. So
//! the mechanical half of that audit lives here, where CI already runs it.
//!
//! # What this cannot check, and what still owns it
//!
//! Everything asserted below is measurable from files in this repository.
//! Against the 38 claims the last audit enumerated, **23 are asserted here** and
//! **15 stay manual**, in four groups:
//!
//! - the **lean-client dependency counts** ("43 crates and 18") — needs
//!   `cargo tree` with and without default features, a subprocess whose answer
//!   depends on how the lock file resolved;
//! - the **toolchain actually in use** — `rustc --version`, a subprocess;
//! - every **state word** — ✅ runs, 🚧 partial, ⛔ not there, and the prose
//!   around them. A tick is a claim with no number attached, which makes it the
//!   easiest kind to leave stale and the hardest to notice;
//! - **claims about behaviour** — that `geo::distance` refuses a shape larger
//!   than a position, that a range on a route takes the scan, that `--help`
//!   exits zero, that `INFO FOR NODE` carries `build` beside `version`. Each is
//!   asserted by the tests of the thing itself. Restating them here would be a
//!   second oracle, free to drift from the first.
//!
//! That list is part of the test. A check that quietly narrows its own scope is
//! how the last audit came to report `23/23` while never opening four files.
//!
//! # Three assertions here are not from that matrix
//!
//! `every_crate_carries_the_same_metadata_set`,
//! `both_documents_state_the_version_this_package_carries` and
//! `the_documented_node_reports_the_prerelease_beside_the_ordered_version` hold
//! release criteria rather than enumerated claims, and they **do not** move any
//! of the fifteen — those still need a subprocess or are asserted by the tests
//! of the thing itself. The split over the matrix stays 23 and 15; these are in
//! addition to it, and are recorded here so the next audit does not count them
//! twice.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root — this crate is `crates/tessaridb`, two levels down.
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

/// The value of a shields.io badge, which is the segment after its label.
///
/// Parsed from the URL rather than the link text because the URL is the thing
/// a reader sees rendered — a badge whose alt text and image disagree shows the
/// image.
fn badge(label: &str) -> String {
    let readme = read("README.md");
    let marker = format!("/badge/{label}-");
    let (_, rest) = readme
        .split_once(&marker)
        .unwrap_or_else(|| panic!("no `{label}` badge in README.md"));
    // shields.io escapes a literal hyphen as `--`, so the value ends at the
    // first *single* hyphen — the one separating it from the colour. Splitting
    // on the first hyphen of any kind truncates `BUSL--1.1` to `BUSL`, so the
    // escaped pairs are masked out of the way first.
    const MASK: char = '\u{0}';
    rest.replace("--", &MASK.to_string())
        .split('-')
        .next()
        .expect("a badge value before its colour")
        .replace(MASK, "-")
        .replace("%20", " ")
}

/// How many cases each corpus file defines.
fn cases_per_file() -> Vec<(String, usize)> {
    let corpus = repo().join("crates/tessari-conformance/tests/corpus");
    let mut counted: Vec<(String, usize)> = fs::read_dir(&corpus)
        .expect("the corpus directory")
        .filter_map(|entry| {
            let path = entry.expect("a corpus entry").path();
            if path.extension()? != "tessariql" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            let cases = fs::read_to_string(&path)
                .expect("a corpus file")
                .lines()
                .filter(|line| line.starts_with("--- case"))
                .count();
            Some((name, cases))
        })
        .collect();
    counted.sort();
    counted
}

#[test]
fn the_conformance_badge_is_the_corpus() {
    let claimed = badge("conformance");
    let counted: usize = cases_per_file().iter().map(|(_, cases)| cases).sum();
    assert_eq!(claimed, format!("{counted} cases"));
}

#[test]
fn the_changelog_counts_the_same_cases_the_badge_does() {
    // Two documents stating one number is two chances for it to be wrong, and
    // this one is stated in prose where no badge draws the eye to it.
    let counted: usize = cases_per_file().iter().map(|(_, cases)| cases).sum();
    let claimed = format!("{counted} conformance cases");
    assert!(
        read("CHANGELOG.md").contains(&claimed),
        "the CHANGELOG does not say `{claimed}`"
    );
}

/// The corpora the engines table does not name, because they are cross-cutting
/// rather than the work of one engine.
const CROSS_CUTTING: [&str; 26] = [
    // A span of identities is an access path over the table's own key order,
    // and every table has one — a log, an audit trail, a session table and a
    // queue all want the same window and none of them is a different engine for
    // wanting it. Filing it under an engine would make the engine that happened
    // to be written first look like its owner.
    "spans",
    // Who may do what, and how far it reaches, is asked before any engine is
    // chosen and answered the same whichever one runs: a `read` at a namespace
    // covers a scan, an index read, a walk and a nearest-neighbour read alike.
    // It sits beside `grants` for that reason — one says which tables, the other
    // says how much of the store, and neither is an access path.
    "authorities",
    "bindings",
    // A view is a name for a read, so it belongs to no engine and to all of
    // them: `DEFINE VIEW v AS SELECT …` names a scan, an index read, a walk or a
    // nearest-neighbour read alike, and expands into whichever the read it
    // stands for was. Counting it under an engine would file it with whichever
    // one the example happened to use.
    "views",
    // Declaring a table's fields with the table is a spelling, and a spelling
    // belongs to no engine: the columns desugar into the field declarations the
    // long form makes, and what a field then does to a write is the same
    // whether the table is scanned, indexed or walked.
    "columns",
    // The vocabulary is cross-cutting by construction: a function is a value's
    // value, and every engine hands over values. `type::int(count)` reads the
    // same whether the record came from a scan, an index, a walk or a
    // nearest-neighbour read, and the same is true of the calendar readings and
    // of everything that opens an object or reshapes an array.
    "calendar",
    "casts",
    "collections",
    // Where a page begins belongs to no engine: `AFTER` resumes a scan, an
    // index read and a walk from the same anchor, and the seek that makes it
    // cheap is a property of the keyspace rather than of an access path.
    "cursor",
    "conditionals",
    "consumers",
    // A fold reduces records to an answer and does not care where the records
    // came from: `median(price)` reads the same over a scan, an index read and a
    // walk. What the two collecting folds *do* belong to is the memory ceiling,
    // which is not an engine either.
    "folds",
    "functions",
    "grants",
    // Which point in the store's history a read answers from is a property of
    // the snapshot, not of an access path. `VERSION` is in fact the clause
    // that takes every engine but the scan *away* — an index describes the
    // present, so a historical read falls back — which makes it the opposite
    // of an engine feature.
    "history",
    // A generated identifier is a value like any other, and every engine
    // hands over values: `rand::uuid()` reads the same whether the record it
    // is written beside came from a scan, an index or a walk. What it does
    // belong to is the planner, which is not an engine either.
    "identity",
    // Undeclaring a catalog object is about the catalog and about no engine:
    // `DROP DATABASE` refuses on the same ground whether the tables inside it
    // are scanned, indexed or walked, and `ALTER TABLE` changes a rule that
    // every engine then writes under.
    "management",
    "node",
    // How many records a read says it answers with is about no engine either:
    // `ONLY` asserts the same thing of a scan, a walk and an index read.
    "only",
    // What a read answers *with* is about no engine in particular: `SELECT *,
    // x AS n` and `OMIT` shape a scan, a walk and a nearest-neighbour read the
    // same way.
    "projections",
    // The plan a read reports and the clause that asserts it are about every
    // engine and the work of none: `USING graph` and `USING index` are the same
    // question asked of two of them.
    "plans",
    "refusals",
    // Opening an array into a row per element belongs to no engine either: the
    // records a walk or a nearest-neighbour read hands over split the same way
    // a scan's do.
    "split",
    // A ceiling on how long a read may run belongs to no engine either: the same
    // clause bounds a scan, a walk and a nearest-neighbour read.
    "timeouts",
    "transactions",
    "users",
];

#[test]
fn the_engines_table_accounts_for_every_case_the_badge_counts() {
    // The `Cases` column holds bare numbers in a column, sometimes summed —
    // `38 + 59`. That is precisely the shape a claims audit built from a text
    // pattern skipped in wave 105: nothing beside the number says what it is.
    let readme = read("README.md");
    let mut tabled = 0usize;
    for line in readme.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // A leading and a trailing empty cell, so the row is
        // ["", Engine, What it gives you, Cases, State, ""].
        if cells.len() != 6 {
            continue;
        }
        if !cells[4].contains("runs") && !cells[4].contains("partial") {
            continue;
        }
        for part in cells[3].split('+') {
            let cases = part
                .trim()
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("not a case count: {:?}", cells[3]));
            tabled = tabled.saturating_add(cases);
        }
    }
    assert!(tabled > 0, "the engines table was not found");

    let per_file = cases_per_file();
    let cross: usize = per_file
        .iter()
        .filter(|(name, _)| CROSS_CUTTING.contains(&name.as_str()))
        .map(|(_, cases)| cases)
        .sum();
    let total: usize = per_file.iter().map(|(_, cases)| cases).sum();

    assert_eq!(
        tabled.saturating_add(cross),
        total,
        "the engines table sums to {tabled}, the cross-cutting corpora to \
         {cross}, and the corpus holds {total}"
    );
}

#[test]
fn the_version_badge_is_this_package() {
    assert_eq!(badge("version"), env!("CARGO_PKG_VERSION"));
}

#[test]
fn both_documents_state_the_version_this_package_carries() {
    // The badge is one of five places a reader learns the version, and it is the
    // only one a build had an opinion about. These two are prose, which is the
    // category that goes stale silently.
    let version = env!("CARGO_PKG_VERSION");
    let heading = read("CHANGELOG.md")
        .lines()
        .find(|line| line.starts_with("## "))
        .expect("a release heading in CHANGELOG.md")
        .to_owned();
    assert!(
        heading.starts_with(&format!("## {version} ")),
        "CHANGELOG.md opens on `{heading}`, and this package is {version}"
    );
    assert!(
        read("README.md").contains(&format!("`{version}`")),
        "README.md's status stage line no longer names {version}"
    );
}

#[test]
fn the_documented_node_reports_the_prerelease_beside_the_ordered_version() {
    // ADR-0034: a pre-release is exact where a person reads it and ordered where
    // it is compared, so a node reports both — `build` with the suffix, `version`
    // with the three numbers that sort. The running node is asserted by its own
    // tests; what drifts unnoticed is the example in the language reference,
    // which is where a reader learns to expect two fields rather than one.
    let build = env!("CARGO_PKG_VERSION");
    let ordered = build.split('-').next().expect("a three-number prefix");
    let docs = read("docs/tessariql.md");
    assert!(
        docs.contains(&format!("\"build\": \"{build}\"")),
        "docs/tessariql.md does not show `build` as {build}"
    );
    assert!(
        docs.contains(&format!("\"version\": \"{ordered}\"")),
        "docs/tessariql.md does not show `version` as {ordered}"
    );
}

#[test]
fn the_rust_badge_is_the_declared_minimum() {
    let manifest = read("Cargo.toml");
    let declared = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version = "))
        .expect("rust-version in the workspace manifest")
        .trim()
        .trim_matches('"');
    assert_eq!(badge("rust"), format!("{declared}%2B"));
}

#[test]
fn the_licence_badge_is_the_licence_file() {
    let named = read("LICENSE");
    let first = named.lines().next().expect("a licence").trim();
    assert_eq!(first, "Business Source License 1.1");
    assert_eq!(badge("licence"), "BUSL-1.1");
}

/// Every crate the architecture section lists, from the fenced block.
fn listed_crates() -> BTreeSet<String> {
    let readme = read("README.md");
    let (_, block) = readme
        .split_once("\ncrates/\n")
        .expect("the architecture listing");
    let (listing, _) = block.split_once("```").expect("the listing is fenced");
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

fn crates_on_disk() -> BTreeSet<String> {
    fs::read_dir(repo().join("crates"))
        .expect("the crates directory")
        .filter_map(|entry| {
            let path = entry.expect("a crate entry").path();
            path.join("Cargo.toml")
                .is_file()
                .then(|| path.file_name()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect()
}

#[test]
fn the_architecture_listing_is_the_workspace() {
    // The count in this list drifted from 17 to 18 without the sentence above
    // it changing, twice — once when a crate was added and once when a
    // dependency upstream split in two.
    assert_eq!(listed_crates(), crates_on_disk());
}

#[test]
fn nothing_is_published_and_every_crate_says_so() {
    // The workspace declares it once and every member inherits, so the claim
    // "every crate carries `publish = false`" is true of a member that says
    // `publish.workspace = true` — and a member saying *neither* is publishable
    // by default, which is the case this exists to catch.
    assert!(
        read("Cargo.toml").contains("\npublish = false"),
        "the workspace no longer declares `publish = false`"
    );
    for name in crates_on_disk() {
        let manifest = read(&format!("crates/{name}/Cargo.toml"));
        // Line by line, and only lines that are not comments. `contains` over
        // the whole file accepts `# publish.workspace = true` — a crate that is
        // publishable and reads as though it says so. That is not a
        // hypothetical: it is how this assertion failed to fail when the claim
        // it checks was deliberately broken.
        let declared = manifest
            .lines()
            .map(str::trim)
            .any(|line| line == "publish = false" || line == "publish.workspace = true");
        assert!(
            declared,
            "{name} neither declares nor inherits `publish = false`, which README \
             and CHANGELOG both claim of every crate"
        );
    }
}

/// The metadata every crate takes from the workspace rather than restating.
///
/// `publish` is deliberately absent: it has its own assertion above, with a
/// looser contract — a crate saying `publish = false` outright satisfies the
/// claim the documents make, and requiring inheritance there would hold the
/// manifests to something nothing claims of them.
const INHERITED: [&str; 6] = [
    "version",
    "edition",
    "rust-version",
    "license",
    "authors",
    "repository",
];

#[test]
fn every_crate_carries_the_same_metadata_set() {
    // A registry rejects a crate missing a field, one crate at a time, at the
    // moment of publishing — which is the worst moment to find out that the
    // eighteenth manifest was written by hand and the other seventeen were
    // copied. The set is small, uniform, and nothing checks it.
    for name in crates_on_disk() {
        let manifest = read(&format!("crates/{name}/Cargo.toml"));
        let lines: Vec<&str> = manifest.lines().map(str::trim).collect();
        for field in INHERITED {
            let inherited = format!("{field}.workspace = true");
            assert!(
                lines.contains(&inherited.as_str()),
                "crates/{name}/Cargo.toml does not inherit `{field}` from the workspace"
            );
        }
        // Every other field is the same in all eighteen; this is the one each
        // crate has to say for itself, which makes it the one that gets left out.
        // The value between the first pair of quotes, rather than the line with
        // its quotes trimmed off the ends: `description = "" # still to write`
        // is an empty description with a comment after it, and trimming quotes
        // from both ends hands back the comment and calls it a description.
        let described = lines
            .iter()
            .find_map(|line| line.strip_prefix("description = "))
            .and_then(|value| value.trim().strip_prefix('"'))
            .and_then(|value| value.split('"').next())
            .unwrap_or_default();
        assert!(
            !described.is_empty(),
            "crates/{name}/Cargo.toml carries no description of its own"
        );
    }
}

#[test]
fn the_absences_that_are_a_missing_crate_stay_missing() {
    // "⛔ Not there: sharding, replication, and cluster membership."
    for name in crates_on_disk() {
        assert!(
            !(name.contains("cluster") || name.contains("shard") || name.contains("replica")),
            "{name} exists, and both documents say the machinery does not"
        );
    }
}

#[test]
fn live_select_is_still_not_written() {
    let language = repo().join("crates/tessari-ql/src");
    for entry in fs::read_dir(&language).expect("the language crate") {
        let path = entry.expect("a source entry").path();
        if path.extension().is_none_or(|kind| kind != "rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("a source file");
        assert!(
            !source.contains("LiveSelect"),
            "{} mentions LiveSelect, and the README says it is not built",
            path.display()
        );
    }
}

/// The `p50 µs` cell of one row of the restore benchmark.
fn recorded_micros(phase: &str) -> f64 {
    let recorded = read("benchmarks/2026-08-22-macos-aarch64-disk.md");
    let section = recorded
        .split("\n## ")
        .find(|part| part.starts_with("restore\n"))
        .expect("the restore section");
    for line in section.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // | phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
        if cells.len() == 9 && cells[1] == phase {
            return cells[4].parse().expect("a p50 in microseconds");
        }
    }
    panic!("no `{phase}` row in the restore benchmark");
}

#[test]
fn the_backup_timings_are_the_baseline_they_cite() {
    // The README attributes these to the benchmarks directory by name, so the
    // benchmarks directory is what they have to equal. Wave 104 corrected them
    // to a *fresh* measurement and left the citation pointing at a file that
    // said something else — a claim with evidence attached that did not support
    // it, which is harder to catch than one with no evidence at all.
    let readme = read("README.md");
    for (phase, verb) in [("backup", "to write"), ("restore", "to replay")] {
        let claimed = format!("{:.1} ms {verb}", recorded_micros(phase) / 1000.0);
        assert!(
            readme.contains(&claimed),
            "the README does not say `{claimed}`, which is what the benchmarks record"
        );
    }
}

/// The `p50 µs` cell of one row of the paging benchmark.
fn paging_micros(phase: &str) -> f64 {
    let recorded = read("benchmarks/2026-08-28-macos-aarch64-memory-paging.md");
    let section = recorded
        .split("\n## ")
        .find(|part| part.starts_with("paging\n"))
        .expect("the paging section");
    for line in section.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // | phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
        if cells.len() == 9 && cells[1] == phase {
            return cells[4].parse().expect("a p50 in microseconds");
        }
    }
    panic!("no `{phase}` row in the paging benchmark");
}

/// A duration as the two paging tables write it: microseconds while they are
/// small, then milliseconds, and rounded half-up because that is what a person
/// transcribing a table does. `{:.0}` would round 12.5 to 12.
fn as_written(micros: f64) -> String {
    let half_up = |value: f64| (value + 0.5).floor();
    if micros < 1000.0 {
        format!("{} µs", half_up(micros))
    } else if micros < 10_000.0 {
        format!("{} ms", half_up(micros / 100.0) / 10.0)
    } else {
        format!("{} ms", half_up(micros / 1000.0))
    }
}

#[test]
fn the_paging_table_is_the_baseline_it_cites() {
    // Both documents print this table and both name the file it came from, and
    // in the commit that added all three they disagreed with it in eleven of
    // sixteen cells — not by rounding, since one cell was low and the rest high,
    // but because a different run was transcribed than the one committed. That
    // is the class the backup guard above was written for; it was never extended
    // to the table added six days later, so this is the same guard for the same
    // reason.
    for document in ["docs/tessariql.md", "benchmarks/README.md"] {
        let text = read(document);
        for (depth, bold) in [(0, false), (1000, false), (10_000, false), (99_000, true)] {
            let cell = |prefix: &str| as_written(paging_micros(&format!("{prefix} at {depth}")));
            let sought = cell("cursor");
            let row = format!(
                "| {} | {} | {} | {} |",
                cell("offset"),
                if bold {
                    format!("**{sought}**")
                } else {
                    sought
                },
                cell("ordered offset"),
                cell("ordered cursor (walked)"),
            );
            assert!(
                text.contains(&row),
                "{document} has no row `{row}` at depth {depth}, \
                 which is what the paging baseline records"
            );
        }
    }
}

/// Every absence claim in a document, by the id it carries.
///
/// The ids are HTML comments — invisible rendered, and the only way two
/// documents can be compared on what they say is *missing* rather than on what
/// they say exists. Prose cannot be diffed; a set of ids can.
fn absences(document: &str) -> BTreeSet<String> {
    read(document)
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("<!-- absent:")?;
            Some(rest.trim_end_matches("-->").trim().to_owned())
        })
        .collect()
}

#[test]
fn the_readme_and_the_changelog_agree_on_what_is_missing() {
    // This is the class the knowledge base records as going stale in the one
    // direction nobody re-reads. Wave 113 edited a bullet's counts and left the
    // absence clause four lines below it untouched, in the same bullet; wave
    // 114 found the two documents disagreeing about `geo::touches` because one
    // list had been updated and the other had not.
    let readme = absences("README.md");
    let changelog = absences("CHANGELOG.md");
    assert!(!readme.is_empty(), "no absence markers in README.md");
    assert_eq!(
        readme,
        changelog,
        "only in README: {:?} — only in CHANGELOG: {:?}",
        readme.difference(&changelog).collect::<Vec<_>>(),
        changelog.difference(&readme).collect::<Vec<_>>()
    );
}

/// `crypto::md5` and `crypto::sha1` are checksums, and nothing that decides
/// whether two things are the same may call them.
///
/// Both are in the language on purpose — a store interoperates, and an ETag or
/// a legacy row key computed by something older than this database has to be
/// reachable from here. The doc comment on `Function::CryptoMd5` says as much,
/// and says the rest of it too: collisions in both are producible on a laptop,
/// so neither may decide whether two things are the same when somebody might
/// want them to appear so.
///
/// That was a sentence in a doc comment, which is exactly the kind of rule the
/// next person adds a call in violation of without ever reading it. This is the
/// sentence as a test.
///
/// The list is the paths where a collision would be a vulnerability rather than
/// a wrong answer: the vault and its sealing, the catalog rows that hold
/// credentials, and the two session paths that verify one. Each path is
/// asserted to EXIST before it is read — a guard over a file that has been
/// renamed guards nothing and passes quietly, which is the failure this shape
/// is most prone to.
#[test]
fn no_security_path_computes_a_broken_digest() {
    const GUARDED: &[(&str, &str)] = &[
        (
            "crates/tessari-storage/src/vault.rs",
            "the vault's own reads and writes",
        ),
        (
            "crates/tessari-storage/src/sealing.rs",
            "sealing and unsealing",
        ),
        (
            "crates/tessari-storage/src/catalog/vault.rs",
            "the vault's catalog rows",
        ),
        (
            "crates/tessari-storage/src/catalog/system.rs",
            "where credentials are hashed",
        ),
        (
            "crates/tessari-session/src/ticket.rs",
            "the session credential",
        ),
        (
            "crates/tessari-session/src/session.rs",
            "where a statement is authorized",
        ),
    ];
    // The spellings a call would take: the language's own, and the two helpers
    // in `tessari-session::digest` that back them.
    const BROKEN: &[&str] = &[
        "crypto::md5",
        "crypto::sha1",
        "CryptoMd5",
        "CryptoSha1",
        "digest::md5",
        "digest::sha1",
    ];

    let mut found = Vec::new();
    for (path, why) in GUARDED {
        let full = repo().join(path);
        assert!(
            full.is_file(),
            "{path} is guarded and does not exist — {why}"
        );
        let text = fs::read_to_string(&full).expect("a guarded path");
        for spelling in BROKEN {
            if text.contains(spelling) {
                found.push(format!("{path} names {spelling} ({why})"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "a broken digest reached a path where a collision is a vulnerability: {found:#?}"
    );
}
