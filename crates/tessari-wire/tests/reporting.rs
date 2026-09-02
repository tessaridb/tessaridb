//! What a node says about a connection while it serves it.
//!
//! A logger is process-wide and installable exactly once, so every assertion
//! about what was reported lives in this one file and runs against one captured
//! stream. Splitting them across files would mean whichever ran first installed
//! the logger and the rest saw nothing — which would pass, silently, and assert
//! nothing at all.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex, OnceLock};

use tessari_wire::{Client, Node};
use tessaridb::Db;

/// Everything the node reported, in order.
static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

fn captured() -> Arc<Mutex<Vec<String>>> {
    Arc::clone(LINES.get_or_init(|| Arc::new(Mutex::new(Vec::new()))))
}

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        captured()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{} {}", record.level(), record.args()));
    }

    fn flush(&self) {}
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
        log::set_boxed_logger(Box::new(Capture)).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
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
    drop(std::thread::spawn(move || held.serve()));
    (node, address)
}

/// The connection number in a line that names one, if it does.
fn connection_in(line: &str) -> Option<u64> {
    let rest = line.split_once("connection ")?.1;
    let number = rest.split_whitespace().next()?;
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
        .find_map(|line| connection_in(line).filter(|_| line.contains("accepted from")))
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
        mine.iter().any(|line| line.contains("accepted from")),
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
        held.contains("sign-in refused for ada"),
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
    drop(std::thread::spawn(move || held.serve()));

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
