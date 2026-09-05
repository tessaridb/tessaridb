//! An index written by an older release, read after the writer was killed
//! (G014 F7).
//!
//! # The criterion, and what is already true
//!
//! F7 asks that every format change carry a read path for data written by the
//! previous version, **or** an explicit refusal naming the version — never a
//! reader that "mostly works" on an old file. Three format changes exist in the
//! search surface and all three already carry that branch:
//!
//! - a **posting** written before the payload existed is the header and nothing
//!   after it, and decodes as `Posting::Membership`;
//! - a **dictionary entry** written before the pruning bound existed ends after
//!   its count, decodes with the zero pair, and answers `None` to `bound()`,
//!   which a reader must take as *do not prune*;
//! - a value carrying an **unknown codec version** is refused outright, and the
//!   refusal names the version it found and the one this build supports.
//!
//! So the mechanism is not what was missing. What was missing is the evidence,
//! and specifically the *shape* of it: every existing test of those read paths
//! is a round trip inside one process against an in-memory backend. That proves
//! the decoder branches. It does not prove that an index written by an older
//! release survives a restart, which is what the criterion asks for and what an
//! operator actually does.
//!
//! # Why the writer is killed rather than closed
//!
//! It is arguable that a signal kill adds nothing to a *format* question: the
//! value bytes are opaque to the engine, and the decode happens above the
//! key-value layer whether the bytes arrived by clean flush or by log replay.
//! The criterion says kill-and-reopen, and rewording a criterion so that the
//! cheaper thing passes it is not available here — so the child is killed, and
//! the argument is recorded rather than acted on. The harness is the one
//! `durability.rs` already uses: the test binary re-executes itself at an
//! ignored test, announces on stdout, and the parent ends it with an uncatchable
//! signal once the announcements it needs have arrived.
//!
//! # What makes the assertions non-vacuous
//!
//! An equality that would hold whichever branch ran proves nothing. So each read
//! is asserted twice over: that the reopened index is **provably** in the prior
//! format — every posting decodes as `Membership`, every dictionary entry
//! answers `None` to `bound()` — and that the answer is **provably** right,
//! identical to the same query's answer taken before the downgrade and announced
//! by the child while the index was still current.
//!
//! That is still not enough for the dictionary row, because an answer can be
//! unchanged for the boring reason that nothing about it ever depended on the
//! bound. So a **control** ships beside the equivalence and asserts that on this
//! corpus a wrong bound *does* move the answer. It earned its place immediately:
//! the first fixture written here was the wide-margin shape W69 had already
//! recorded as untestable, and the control caught it in one run.
//!
//! # One thing the `MATCHES` row does not prove
//!
//! `Transaction::postings` reads posting *keys* and never their values, so a
//! membership-only index answers `MATCHES` because the values are irrelevant to
//! that path — not because a read path recovered anything from them. That is the
//! format's stated promise and worth pinning, but the row that actually
//! exercises the `Membership` read path is the **scored** one, which goes
//! through `Transaction::posting` and decodes.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tessari_encoding::{
    CODEC_VERSION, Posting, PostingKey, SearchTermKey, StoreKey, StoreValue, TermStatistics,
};
use tessari_kv::{KeyRange, Keyspace, KvBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_lsm::{Durability, LsmBackend, StoreConfig};
use tessari_session::Session;
use tessari_storage::Store;

/// Environment variable carrying the store path into the child.
const STORE_PATH: &str = "TESSARIDB_FORMAT_STORE";

/// How many records the table holds — half holding one word, half the other.
const RECORDS: u32 = 20;

/// How many of the ranked read's answers are compared.
///
/// Six, because the fixture is built so that the leading six draw from **both**
/// groups; see [`build`].
const WANTED: usize = 6;

/// Where the child gives up on its own, so it cannot outlive a parent that died
/// before killing it. The parent ends it long before this.
const CHILD_GIVES_UP_AFTER: u32 = 2_000;

/// The read whose answer is a set: does the index still say which records hold
/// the word.
const MEMBERS: &str = "SELECT * FROM notes WHERE body MATCHES 'alpha';";

/// The read whose answer is an order: the bounded ranked read, which is the one
/// the dictionary's pruning bound serves.
const RANKED: &str = "SELECT * FROM notes ORDER BY search::score(body, 'alpha beta') DESC LIMIT 6;";

/// The store, and the backend under it.
///
/// The backend is handed back rather than reached for through the store, because
/// the store keeps it to itself — deliberately, and this test is the exception
/// that proves it rather than a reason to widen the type. Downgrading an index
/// is not something a caller does; it is something an older release did, and
/// this is the only way to be that release.
fn open(path: &std::path::Path) -> (Arc<dyn KvBackend>, Store) {
    let backend = LsmBackend::open(path, StoreConfig::new(Durability::PowerLossSafe)).unwrap();
    let backend = Arc::new(backend) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// The corpus both the child and the control build, so the two cannot drift.
///
/// **Two words of equal weight, held by different records, both belonging in the
/// answer.** The shape is deliberate and it is the one W69 had to learn: a
/// fixture where a handful of records out-score the rest by an order of
/// magnitude cannot see a bound that is wrong in the unsafe direction, because
/// pruning ten times too hard still only discards records that could never have
/// won. The first fixture written here was exactly that, and the control below
/// caught it — a deliberately absurd bound changed nothing. So the two terms are
/// in ten records each, and neither group can be dropped without losing rows the
/// answer wants.
fn build(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;",
        )
        .unwrap();
    for n in 1..=RECORDS {
        let word = if n <= RECORDS / 2 { "alpha" } else { "beta" };
        let repeats = usize::try_from(n % 4).unwrap().saturating_add(1);
        let body = vec![word; repeats].join(" ");
        session
            .run(&format!("CREATE notes:{n} = {{ body: '{body}' }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();
}

/// Whether a ranked answer draws from **both** halves of the corpus.
///
/// The premise the contest rests on, asserted rather than assumed: an answer
/// that quietly came from one group alone would make every equality below hold
/// for the wrong reason.
fn contested(answer: &[String]) -> bool {
    let low = answer
        .iter()
        .filter(|id| id.parse::<u32>().is_ok_and(|n| n <= RECORDS / 2))
        .count();
    low > 0 && low < answer.len()
}

/// The record ids one read answers with, in the order it answered them.
///
/// Ids rather than whole records, because the downgrade touches the index and
/// never the records — what is being asserted is which records the index reaches
/// and in what order, not what they contain.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<String> {
    session
        .run(read)
        .unwrap()
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect()
}

/// Every entry of one kind in the index keyspace, as raw pairs.
///
/// Decoding is the filter, exactly as `scoring_source.rs` does it: the index
/// keyspace also holds the dictionary, the statistics and every other index's
/// entries, and only the key of the wanted kind decodes as one.
fn entries_of<K: StoreKey>(backend: &dyn KvBackend) -> Vec<(tessari_kv::Key, tessari_kv::Value)> {
    let request = ScanRequest {
        keyspace: Keyspace::INDEX,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    backend
        .scan(&request)
        .unwrap()
        .into_iter()
        .filter(|(key, _)| K::decode(key.as_slice()).is_ok())
        .collect()
}

/// Rewrite every posting as one written before postings carried a payload.
///
/// The keys are untouched, so the index still says exactly which records hold
/// which terms; only the two numbers are taken away, which is the state an index
/// written by an older release is in.
fn forget_the_numbers(backend: &dyn KvBackend) -> usize {
    let found = entries_of::<PostingKey>(backend);
    let mut batch = WriteBatch::default();
    for (key, _) in &found {
        batch = batch.put(Keyspace::INDEX, key.clone(), Posting::Membership.encode());
    }
    backend.apply(batch).unwrap();
    found.len()
}

/// Rewrite every dictionary entry as one written before the pruning bound
/// existed: the header and the count, and nothing after them.
///
/// The truncation is taken from the current encoding rather than written by
/// hand — the header is however many bytes an empty payload occupies, and the
/// count is a `u64` — so this cannot drift away from what the codec writes.
fn forget_the_bound(backend: &dyn KvBackend) -> usize {
    let header = Posting::Membership.encode().as_slice().len();
    let found = entries_of::<SearchTermKey>(backend);
    let mut batch = WriteBatch::default();
    for (key, value) in &found {
        let mut older = value.as_slice().to_vec();
        older.truncate(header.saturating_add(8));
        batch = batch.put(Keyspace::INDEX, key.clone(), tessari_kv::Value::from(older));
    }
    backend.apply(batch).unwrap();
    found.len()
}

/// The child. Builds the corpus, announces what the current format answers,
/// downgrades the whole index, announces that, and then waits to be killed.
///
/// It is `#[ignore]`d because it does not return on its own.
#[test]
#[ignore = "spawned by the format test; runs until it is killed"]
fn build_downgrade_and_wait() {
    let Ok(path) = std::env::var(STORE_PATH) else {
        panic!("{STORE_PATH} must name the store directory");
    };
    let (backend, store) = open(std::path::Path::new(&path));
    let mut session = Session::new(&store);
    build(&mut session);

    // Announced while the index is still in the current format, so the parent's
    // comparison is against what this build answers rather than against a
    // constant written into the test.
    announce("members", &ids(&mut session, MEMBERS));
    announce("ranked", &ids(&mut session, RANKED));

    let postings = forget_the_numbers(backend.as_ref());
    let terms = forget_the_bound(backend.as_ref());
    assert!(postings > 0, "no postings were downgraded");
    assert!(terms > 0, "no dictionary entries were downgraded");
    announce("downgraded", &[postings.to_string(), terms.to_string()]);

    // Nothing left to do but stay alive until the signal arrives.
    for _ in 0..CHILD_GIVES_UP_AFTER {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One line the parent reads. Flushed, because an announcement the parent never
/// reads proves nothing.
fn announce(label: &str, values: &[String]) {
    println!("{label} {}", values.join(" "));
    std::io::stdout().flush().unwrap();
}

/// Read the child's announcements until the last one has arrived.
fn listen(child: &mut std::process::Child) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut heard = std::collections::BTreeMap::new();
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        let mut parts = line.split_whitespace();
        let Some(label) = parts.next() else {
            continue;
        };
        if !matches!(label, "members" | "ranked" | "downgraded") {
            continue;
        }
        let values: Vec<String> = parts.map(str::to_owned).collect();
        heard.insert(label.to_owned(), values);
        if heard.len() == 3 {
            break;
        }
    }
    heard
}

#[test]
fn an_index_in_the_prior_format_answers_the_same_after_its_writer_is_killed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "build_downgrade_and_wait",
            "--ignored",
            "--nocapture",
        ])
        .env(STORE_PATH, &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let heard = listen(&mut child);

    // No unwinding, no destructor, no flush.
    child.kill().unwrap();
    let _ = child.wait();

    assert_eq!(
        heard.len(),
        3,
        "the child died before it finished announcing: {heard:?}"
    );
    let members = heard.get("members").unwrap();
    let ranked = heard.get("ranked").unwrap();
    let downgraded = heard.get("downgraded").unwrap();
    assert!(
        !members.is_empty() && ranked.len() == WANTED && contested(ranked),
        "the reference answers are not the ones the reads promise: {members:?} / {ranked:?}"
    );

    let (backend, store) = open(&path);

    // The state, before the answers: the reopened index is provably the older
    // format, so the equalities below cannot be passing because nothing changed.
    let postings = entries_of::<PostingKey>(backend.as_ref());
    assert_eq!(
        postings.len().to_string(),
        downgraded[0],
        "the reopened store holds a different number of postings than the child wrote"
    );
    for (_, value) in &postings {
        assert_eq!(
            Posting::decode(value.as_slice()).unwrap(),
            Posting::Membership,
            "a posting survived the downgrade with its numbers"
        );
    }
    let terms = entries_of::<SearchTermKey>(backend.as_ref());
    assert_eq!(terms.len().to_string(), downgraded[1]);
    for (_, value) in &terms {
        let read = TermStatistics::decode(value.as_slice()).unwrap();
        assert_eq!(
            read.bound(),
            None,
            "a dictionary entry survived the downgrade with its bound"
        );
        assert!(
            read.documents > 0,
            "the truncation took the count as well, so the entry is not the prior format"
        );
    }

    let mut session = Session::new(&store);
    session.run(USE).unwrap();

    // Membership is what `MATCHES` ever needed, and the index still carries it.
    assert_eq!(&ids(&mut session, MEMBERS), members);

    // The read path that recovers the numbers from the record's text, and the
    // dictionary that declines to prune because it cannot bound its terms.
    assert_eq!(&ids(&mut session, RANKED), ranked);
}

/// **The control.** On *this* fixture, what the dictionary says about a term
/// changes the ranked answer — so the equality above is not holding because the
/// bound was never load-bearing here.
///
/// W69 established that an unsound bound is caught, but it established it on its
/// own fixture, and it also recorded the trap: its first fixture could not fail,
/// because four rare records beating fifty-six by an order of magnitude gives the
/// same answer under any threshold in a wide band. That is a property of a
/// corpus, not of the engine, so it has to be re-established here rather than
/// inherited.
///
/// The bound written here is deliberately far too tight — one occurrence in a
/// record a million tokens long, which is the least any term could contribute —
/// so a reader that believes it will abandon terms it should have walked. The
/// assertion is only that the answer **moves**. Which wrong answer it becomes is
/// not this test's business; that a wrong bound is visible at all is.
#[test]
fn on_this_fixture_a_wrong_bound_moves_the_ranked_answer() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let (backend, store) = open(&path);
    let mut session = Session::new(&store);
    build(&mut session);
    let sound = ids(&mut session, RANKED);
    assert_eq!(sound.len(), WANTED);
    assert!(contested(&sound), "the fixture does not contest: {sound:?}");

    let found = entries_of::<SearchTermKey>(backend.as_ref());
    let mut batch = WriteBatch::default();
    for (key, value) in &found {
        let read = TermStatistics::decode(value.as_slice()).unwrap();
        let wrong = TermStatistics::bounded(read.documents, 1, 1_000_000);
        batch = batch.put(Keyspace::INDEX, key.clone(), wrong.encode());
    }
    backend.apply(batch).unwrap();

    assert_ne!(
        ids(&mut session, RANKED),
        sound,
        "this fixture answers the same whatever the dictionary says, so the equality \
         in the test above proves nothing about the prior format's bound",
    );
}

#[test]
fn a_value_from_an_unknown_codec_version_is_refused_and_names_it() {
    // No kill here: a refusal is a decode, and what the reopen establishes is
    // that the bytes came off a file rather than out of a map the writer still
    // held.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let rewritten = {
        let (backend, store) = open(&path);
        let mut session = Session::new(&store);
        // The same corpus as the others, so the read below is one the index
        // actually serves. A single-record table takes the scan, and a scan is
        // not where a posting's value is read.
        build(&mut session);

        let found = entries_of::<PostingKey>(backend.as_ref());
        let mut batch = WriteBatch::default();
        for (key, value) in &found {
            let mut ahead = value.as_slice().to_vec();
            // A version this build has never written, which is what a file from
            // a future release looks like from here.
            ahead[0] = CODEC_VERSION.saturating_add(1);
            batch = batch.put(Keyspace::INDEX, key.clone(), tessari_kv::Value::from(ahead));
        }
        backend.apply(batch).unwrap();
        found.len()
    };
    assert!(rewritten > 0, "no postings were re-versioned");

    let (_backend, store) = open(&path);
    let mut session = Session::new(&store);
    session.run(USE).unwrap();

    // The scored read is the one that decodes a posting's value; `MATCHES` reads
    // keys alone and would not reach this at all.
    let refused = session.run(RANKED);
    let error = refused.expect_err("an unknown codec version must not be read as this one");

    let said = error.to_string();
    assert!(
        said.contains(&CODEC_VERSION.saturating_add(1).to_string())
            && said.contains(&CODEC_VERSION.to_string()),
        "the refusal must name the version it found and the one this build supports, \
         and it said: {said}",
    );
}
