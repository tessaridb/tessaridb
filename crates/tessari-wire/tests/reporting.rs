//! What a node says about a connection while it serves it, and about each pass
//! of the cadences it runs.
//!
//! A logger is process-wide and installable exactly once, so every assertion
//! about what was reported lives in this one file and runs against one captured
//! stream. Splitting them across files would mean whichever ran first installed
//! the logger and the rest saw nothing — which would pass, silently, and assert
//! nothing at all.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::sync::{Arc, Mutex, OnceLock};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tessari_wire::{Client, Node, PassFailed, every, every_paced};
use tessaridb::Db;
use tokio_util::sync::CancellationToken;

/// Everything the node reported, in order.
static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

fn captured() -> Arc<Mutex<Vec<String>>> {
    Arc::clone(LINES.get_or_init(|| Arc::new(Mutex::new(Vec::new()))))
}

/// A writer that keeps each formatted event as one line of the capture.
///
/// The formatter writes a whole event in one call, so one call is one line.
struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, written: &[u8]) -> std::io::Result<usize> {
        captured()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(String::from_utf8_lossy(written).trim().to_owned());
        Ok(written.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Nothing here runs beside anything else here.
///
/// The connection number is a process-wide counter and the capture is a
/// process-wide buffer, so two of these tests running at once interleave into
/// one stream and each sees the other's connections. That does not fail — it
/// *passes*, against the wrong lines, which is worse. One at a time, with the
/// buffer cleared, is what makes each assertion about the connection its own
/// test made.
static ALONE: Mutex<()> = Mutex::new(());

/// Install the capturing logger and take the floor, clearing what came before.
///
/// Returns the guard: hold it for the body of the test.
fn listening() -> std::sync::MutexGuard<'static, ()> {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(|| Capture)
            .init();
    });
    // A test that panicked while holding this poisoned it; the floor is still
    // free and the next test's assertions are still its own.
    let floor = ALONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    captured()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    floor
}

/// Spin until `settled` holds, or give up and let the assertion say so.
///
/// The work happens on the node's own thread, so "it has not happened yet" and
/// "it will never happen" look identical at the instant a test asks.
fn until(settled: impl Fn() -> bool) -> bool {
    (0..100_000).any(|_| {
        if settled() {
            return true;
        }
        std::thread::yield_now();
        false
    })
}

fn lines() -> Vec<String> {
    captured()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// A node on a loopback port the operating system picked, plus its address.
fn serving(db: Db) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(Arc::new(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || serve_until_the_test_ends(&held)));
    (node, address)
}

/// The connection number in a line that names one, if it does — as the
/// `connection=` field of the accept line, or of the span every later line of
/// that connection is reported inside.
fn connection_in(line: &str) -> Option<u64> {
    let rest = line.split_once("connection=")?.1;
    let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
    number.parse().ok()
}

#[test]
fn one_conversation_is_reported_from_accept_to_close_under_one_name() {
    let _floor = listening();
    let (_node, address) = serving(Db::in_memory().unwrap());

    // A credential nobody declared, so the node refuses it. The refusal is the
    // middle of the three lines and the one an operator is actually looking for.
    let mut client = Client::connect(&address).unwrap();
    let refused = client.run("SELECT * FROM users;", Some(("ada", "not the one")));
    assert!(refused.is_err(), "a refused sign-in must refuse the script");
    drop(client);

    // Identified by its **accept** line, not by its close line. Only this test
    // connects while it holds the floor, so the accept is certainly ours — while
    // a close can arrive from a node an earlier test left running, after the
    // buffer was cleared and while this one is watching it. Reading the id off a
    // close line makes this test pass or fail on another test's timing, which is
    // exactly the mistake the floor was taken to avoid.
    let id = lines()
        .iter()
        .find_map(|line| connection_in(line).filter(|_| line.contains("connection accepted")))
        .expect("the node should report the connection arriving");
    assert!(
        until(|| lines()
            .iter()
            .any(|line| connection_in(line) == Some(id) && line.contains("closed"))),
        "connection {id} should have been reported closing: {:?}",
        lines()
    );

    let mine: Vec<String> = lines()
        .into_iter()
        .filter(|line| connection_in(line) == Some(id))
        .collect();

    assert!(
        mine.iter().any(|line| line.contains("connection accepted")),
        "no accept line: {mine:?}"
    );
    assert!(
        mine.iter()
            .any(|line| line.starts_with("WARN") && line.contains("refused")),
        "no refusal line at WARN: {mine:?}"
    );
    assert!(
        mine.iter().any(|line| line.contains("closed")),
        "no close line: {mine:?}"
    );
}

#[test]
fn a_refused_sign_in_reports_the_name_and_never_the_password() {
    let _floor = listening();
    let (_node, address) = serving(Db::in_memory().unwrap());

    let secret = "correct horse battery staple";
    let mut client = Client::connect(&address).unwrap();
    drop(client.run("SELECT * FROM users;", Some(("ada", secret))));
    drop(client);

    let held = lines().join("\n");
    assert!(
        lines()
            .iter()
            .any(|line| line.contains("sign-in refused") && line.contains("user=ada")),
        "the name is what makes a refusal followable: {held}"
    );
    assert!(
        !held.contains(secret),
        "the password must not reach a log line: {held}"
    );
}

#[test]
fn a_client_that_says_nothing_is_let_go_rather_than_holding_a_thread() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let _floor = listening();
    let (_node, address) = serving(Db::in_memory().unwrap());

    // Connect, greet nobody, send not one byte. Before the deadline existed this
    // held a node thread for the life of the process, at a cost to the client of
    // one socket and no traffic.
    let mut silent = TcpStream::connect(&address).unwrap();
    silent
        .set_read_timeout(Some(Duration::from_secs(
            tessari_constants::GREETING_SECONDS + 20,
        )))
        .unwrap();

    let began = Instant::now();
    let mut said = Vec::new();
    // The node greets first, then waits for ours. When its deadline passes it
    // closes, and this read ends — with nothing more, or with an error.
    drop(silent.read_to_end(&mut said));
    let waited = began.elapsed();

    assert!(
        waited < Duration::from_secs(tessari_constants::GREETING_SECONDS + 15),
        "the node should have let go after its deadline, and waited {waited:?}"
    );
}

#[test]
fn a_place_at_the_door_is_taken_for_a_connection_and_given_back_after_it() {
    let _floor = listening();
    let node = Arc::new(Node::bind(Arc::new(Db::in_memory().unwrap()), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let door = node.door();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || serve_until_the_test_ends(&held)));

    assert_eq!(door.open(), 0, "an idle node holds no places");
    assert_eq!(door.limit(), tessari_constants::MAX_CONNECTIONS);

    let clients: Vec<_> = (0..3).map(|_| Client::connect(&address).unwrap()).collect();
    assert!(
        until(|| door.open() == 3),
        "three conversations should hold three places, and {} are held",
        door.open()
    );

    drop(clients);
    assert!(
        until(|| door.open() == 0),
        "a place must come back when its connection ends, and {} are still held",
        door.open()
    );

    // The ceiling itself, and the race for the last places under it, are proven
    // by `tessari-serve`'s own tests: opening MAX_CONNECTIONS + 1 real sockets
    // here would spend four hundred node threads and a file-descriptor limit to
    // re-prove an invariant a unit test already holds. What this test adds is
    // the part those cannot see — that the node actually takes a place, and
    // actually gives it back.
}

#[test]
fn a_session_carried_over_a_websocket_takes_its_place_at_the_same_door() {
    // ADR-0089: `/wire` sessions and TCP connections share one ceiling. A
    // second door would let a node hold twice what it was sized for.
    let node = Node::bind(Arc::new(Db::in_memory().unwrap()), "127.0.0.1:0").unwrap();
    let door = node.door();
    let carrier = node.carrier();

    let held: Vec<_> = std::iter::from_fn(|| carrier.admit())
        .take(door.limit().saturating_add(1))
        .collect();
    assert_eq!(
        held.len(),
        door.limit(),
        "the carrier admitted past the node's own ceiling"
    );
    assert_eq!(door.open(), door.limit(), "carried sessions held no places");
    assert!(carrier.admit().is_none(), "a full door admitted one more");

    drop(held);
    assert_eq!(
        door.open(),
        0,
        "a carried session kept its place after it ended"
    );
}

/// Serve `node` on a runtime of this test's own: the node creates none.
fn serve_until_the_test_ends(node: &Node) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
}

/// A runtime on the real clock, for a budget that has to run out while a pass
/// is still on the blocking pool.
fn real() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// A runtime whose clock moves only when everything on it is waiting, so a
/// cadence's budget and its periods run out at once instead of in real time.
fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap()
}

#[test]
fn a_pass_that_outlasts_its_period_is_reported_and_the_next_one_waits_for_it() {
    // The budget cannot cancel a pass — it runs on the blocking pool — so what
    // the runner owes is to SAY it overran, under the cadence's name, and to
    // keep the passes one after another anyway. On the real clock: a paused one
    // does not move while a blocking task is out, so no budget would ever run
    // out. The pass sleeps fifteen times its budget, which a loaded machine
    // does not close.
    let _floor = listening();
    real().block_on(async {
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let inside = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let passes = Arc::new(AtomicUsize::new(0));
        let (entering, crossed, counted) = (
            Arc::clone(&inside),
            Arc::clone(&overlapped),
            Arc::clone(&passes),
        );
        every("slow-test", Duration::from_millis(20), &stop, move |_| {
            if entering.fetch_add(1, Ordering::SeqCst) > 0 {
                crossed.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(300));
            entering.fetch_sub(1, Ordering::SeqCst);
            if counted.fetch_add(1, Ordering::SeqCst) >= 1 {
                stopping.cancel();
            }
            Ok(())
        })
        .await;
        assert_eq!(passes.load(Ordering::SeqCst), 2);
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "a pass started while the one that overran was still running"
        );
        let said = lines();
        let overran: Vec<_> = said
            .iter()
            .filter(|line| line.contains("overran") && line.contains("slow-test"))
            .collect();
        assert_eq!(
            overran.len(),
            2,
            "each overrun said once, by name: {said:#?}"
        );
        assert!(
            said.iter()
                .any(|line| line.contains("pass completed") && line.contains("slow-test")),
            "a pass that overran and then finished was not reported finished: {said:#?}"
        );
    });
}

#[test]
fn a_pass_that_fails_is_reported_with_its_cadence_and_the_cadence_goes_on() {
    let _floor = listening();
    paused().block_on(async {
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let passes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&passes);
        every("failing-test", Duration::from_secs(1), &stop, move |_| {
            if counted.fetch_add(1, Ordering::SeqCst) >= 1 {
                stopping.cancel();
            }
            Err(PassFailed::new(
                "the test cannot do its work",
                std::io::Error::other("refused on purpose"),
            ))
        })
        .await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            2,
            "a failed pass ended the cadence"
        );
        let said = lines();
        let failed: Vec<_> = said
            .iter()
            .filter(|line| line.contains("pass failed") && line.contains("failing-test"))
            .collect();
        assert_eq!(failed.len(), 2, "{said:#?}");
        assert!(
            failed
                .iter()
                .all(|line| line.contains("the test cannot do its work")
                    && line.contains("refused on purpose")),
            "the failure lost what the pass could not do or why: {failed:#?}"
        );
        assert!(
            !said.iter().any(|line| line.contains("pass completed")),
            "a failed pass was also reported completed: {said:#?}"
        );
    });
}

#[test]
fn a_paced_pass_that_fails_waits_the_last_period_rather_than_running_again_at_once() {
    // A failed paced pass names no next period. Running again at once would
    // turn a node that cannot read its catalog into a loop that does nothing
    // else; the period the cadence last chose is the one it waits instead.
    let _floor = listening();
    paused().block_on(async {
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let wake = tokio::sync::Notify::new();
        let began = tokio::time::Instant::now();
        let at = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&at);
        every_paced(
            "paced-test",
            &stop,
            &wake,
            Duration::from_secs(3600),
            move |_| {
                let mut seen = seen.lock().unwrap();
                seen.push(began.elapsed());
                if seen.len() >= 2 {
                    stopping.cancel();
                }
                Err(PassFailed::new(
                    "the test cannot read its catalog",
                    std::io::Error::other("refused on purpose"),
                ))
            },
        )
        .await;
        let at = at.lock().unwrap();
        assert_eq!(at.len(), 2);
        assert!(
            at[1].saturating_sub(at[0]) >= Duration::from_secs(3600),
            "a failed paced pass ran again after {:?}",
            at[1].saturating_sub(at[0])
        );
    });
}
