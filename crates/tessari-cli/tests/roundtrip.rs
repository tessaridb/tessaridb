//! What the command line prints can be pasted back in.
//!
//! The claim is easy to make and easy to make falsely, because a hand-written
//! example uses four value kinds and the language has seventeen. So the fixture is
//! a record holding one of each, written through the store, read back, rendered,
//! and then written again as a statement — and the two records are compared.
//!
//! What that catches is a kind added to the value system whose rendering nobody
//! wrote: it fails here rather than printing something the parser cannot read.
//! On its first run it caught three — a datetime and a duration were being
//! written in a debugging form the lexer will not read, and a record reference
//! cannot be written at all.
//!
//! A **shape** is in the fixture as of the wave that gave the language a
//! literal for one. It is the kind whose rendering was previously a deliberate
//! exception — the console printed `<geometry point of 1>` precisely so that
//! nobody would paste it — and it is therefore the one most worth holding here
//! now that the exception is gone.
//!
//! **Two of the seventeen are absent from the fixture**, and no longer because
//! they cannot be rendered. A `table` and a `record` hold an id, and the name
//! comes from a resolver the caller supplies (`Db::names_in`) — which this test
//! deliberately does not, because what it is checking is the *value* renderer
//! and a resolver would make it a test of two things. That references render as
//! `users:1` and paste back is asserted where a catalog exists: `routes.rs` for
//! the JSON surface, and by hand for the console.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Value;

/// Every literal form `docs/tessariql.md` §3 lists, in one record.
const ONE_OF_EACH: &str = "CREATE probe:1 = {\n\
    absent:   NONE,\n\
    empty:    NULL,\n\
    yes:      true,\n\
    no:       false,\n\
    whole:    42,\n\
    negative: -7,\n\
    real:     1.5,\n\
    exact:    dec 12.34,\n\
    single:   'text',\n\
    tricky:   'it''s a \\\\ and a ; and a newline',\n\
    raw:      0x0a1b,\n\
    span:     2s,\n\
    longer:   1h30m,\n\
    at:       datetime '1970-01-01T00:00:00Z',\n\
    who:      uuid '00112233-4455-6677-8899-aabbccddeeff',\n\
    list:     [1, 'two', [3]],\n\
    nested:   { inner: { deeper: 1 } },\n\
    keyed:    { 'with space': 1 },\n\
    unique:   set [1, 2, 3],\n\
    place:    geometry { type: 'Point', coordinates: [2.35, 48.85] },\n\
    path:     geometry { type: 'LineString', coordinates: [[-1.5, 0], [1, 2]] },\n\
    zone:     geometry { type: 'Polygon', coordinates: [[[0, 0], [1, 0], [1, 1], [0, 0]]] }\n\
};";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION probe;",
        )
        .unwrap();
    session
}

fn read(session: &mut Session<'_>, id: u64) -> Value {
    let outcomes = session.run(&format!("SELECT * FROM probe:{id};")).unwrap();
    outcomes[0].records().unwrap()[0].1.clone()
}

#[test]
fn every_value_kind_survives_being_printed_and_read_back() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(ONE_OF_EACH.replace("''", "\\'").as_str())
        .unwrap();
    let original = read(&mut session, 1);

    // Render it the way the command line would, and write it back as a second
    // record. If the rendering is not readable, this refuses.
    let rendered = tessari_cli_render(&original);
    session
        .run(&format!("CREATE probe:9 = {rendered};"))
        .unwrap_or_else(|failure| panic!("the rendering did not parse: {failure}\n{rendered}"));
    let again = read(&mut session, 9);

    assert_eq!(original, again, "rendered as:\n{rendered}");

    // And the per-record form the command line actually prints, which puts the
    // id in front of the value.
    let line = render::record("9", &again, &render::Names::new());
    assert!(line.starts_with("9: {"), "{line}");
}

/// The renderer, which lives in the language crate so a state script and the
/// command line write a value the same way (ADR-0091).
use tessari_ql::literal as render;

fn tessari_cli_render(held: &Value) -> String {
    // The fixture holds no reference, by design — see the module note — so an
    // empty resolver is the honest one here.
    render::value(held, &render::Names::new())
}

/// Stores written by released builds, each beside the answers that build gave
/// (`tests/fixtures/released`, made by its `generate.sh` from the published
/// images — G059 C3).
const RELEASED: &[&str] = &[
    "0.22.0-beta",
    "0.23.0-beta",
    "0.24.0-beta",
    "0.25.0-beta",
    "0.26.0-beta",
    "0.27.0-beta",
    "0.27.1-beta",
    "0.28.0-beta",
    "0.29.0-beta",
    "0.30.0-beta",
    "0.31.0-beta",
    "0.31.1-beta",
    "0.31.2-beta",
    "0.32.0-beta",
    "0.33.0-beta",
    "0.33.1-beta",
    "0.33.2-beta",
];

/// The binary this crate builds, which is the one an operator upgrades to.
const TESSARIDB: &str = env!("CARGO_BIN_EXE_tessaridb");

/// The minor number of a `0.MINOR.PATCH-beta` version.
fn minor(version: &str) -> u32 {
    version.split('.').nth(1).unwrap().parse().unwrap()
}

/// `RELEASED` is a list that grows with every release, and nothing else fails
/// when it stops growing: 0.27.0 and 0.27.1 shipped the format promise and were
/// missing from it (G062 G1). So the list must name exactly the stores on disk,
/// and the newest of them may trail this build by one minor at most — the
/// current release cannot be among them before its image is published. It
/// compares minors only, so across a major bump it says nothing — that release
/// checks the fixtures by hand.
#[test]
fn the_released_stores_keep_up_with_the_releases() {
    let mut listed: Vec<&str> = RELEASED.to_vec();
    listed.sort_unstable();
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/released");
    let mut on_disk: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    on_disk.sort_unstable();
    assert_eq!(
        listed, on_disk,
        "RELEASED and tests/fixtures/released disagree"
    );
    let newest = RELEASED.iter().map(|version| minor(version)).max().unwrap();
    let this_build = minor(env!("CARGO_PKG_VERSION"));
    assert!(
        this_build.saturating_sub(newest) <= 1,
        "the newest released store is 0.{newest}; this build is 0.{this_build} — \
         run generate.sh for the releases in between"
    );
}

fn released(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/released")
        .join(name)
}

/// Run one script against `store` with this build and keep what it printed, on
/// either stream.
fn answer(store: &std::path::Path, script: &str) -> String {
    let ran = std::process::Command::new(TESSARIDB)
        .arg(store)
        .arg("-f")
        .arg(released(script))
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{script} failed: {}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    // Notes go to standard error; the records are what decides the comparison,
    // and a note is read only for whether it is there.
    let mut printed = String::from_utf8(ran.stdout).unwrap();
    printed.push_str(&String::from_utf8(ran.stderr).unwrap());
    printed
}

/// What an answer says about the data: every record and every count, without
/// the access path that served it or the notes about how — those are what a
/// newer build is allowed to change.
fn records_and_counts(printed: &str) -> Vec<String> {
    printed
        .lines()
        .filter(|line| !line.starts_with("note:"))
        .map(|line| match line.split_once(", via ") {
            Some((count, _)) if line.starts_with('(') => format!("{count})"),
            _ => line.to_owned(),
        })
        .collect()
}

#[test]
fn a_store_written_by_each_released_build_reads_back_what_that_build_answered() {
    for version in RELEASED {
        let copy = tempfile::tempdir().unwrap();
        let store = copy.path().join("store");
        std::fs::create_dir(&store).unwrap();
        for file in std::fs::read_dir(released(version)).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), store.join(file.file_name())).unwrap();
        }
        let expected = records_and_counts(
            &std::fs::read_to_string(released(&format!("{version}.answers"))).unwrap(),
        );
        assert!(expected.len() > 40, "{version}: the oracle is empty");

        let before = answer(&store, "read.tessariql");
        assert_eq!(
            records_and_counts(&before),
            expected,
            "{version}: this build reads the store differently from the build that wrote it"
        );
        // A search index from before 0.26 recorded no tokenizer, so it was set
        // aside — the records above came from the scan — and the read says so.
        assert_eq!(
            before.contains("recorded no tokenizer generation"),
            minor(version) < 26,
            "{version}: whether the old search index was set aside:\n{before}"
        );
        // Rebuilt by this build, its indexes answer the same records again —
        // through the index this time, where the store's own was set aside.
        answer(&store, "rebuild.tessariql");
        let after = answer(&store, "read.tessariql");
        assert_eq!(
            records_and_counts(&after),
            expected,
            "{version}: the rebuilt indexes answer differently"
        );
        assert!(
            !after.contains("tokenizer generation"),
            "{version}: an index still waits for a rebuild:\n{after}"
        );
    }
}
