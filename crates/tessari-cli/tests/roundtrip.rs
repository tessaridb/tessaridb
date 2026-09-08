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

/// The renderer, reached the way an integration test can reach a binary crate's
/// module: by including the file. A binary has no library target to depend on,
/// and giving it one to make a test possible would be shaping the crate around
/// its test.
#[path = "../src/render.rs"]
mod render;

fn tessari_cli_render(held: &Value) -> String {
    // The fixture holds no reference, by design — see the module note — so an
    // empty resolver is the honest one here.
    render::value(held, &render::Names::new())
}
