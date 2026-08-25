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
            .unwrap()
            .push(format!("{} {}", record.level(), record.args()));
    }

    fn flush(&self) {}
}

/// Install the capturing logger, once for the whole binary.
fn listening() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        log::set_boxed_logger(Box::new(Capture)).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
}

fn lines() -> Vec<String> {
    captured().lock().unwrap().clone()
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
    listening();
    let (_node, address) = serving(Db::in_memory().unwrap());

    // A credential nobody declared, so the node refuses it. The refusal is the
    // middle of the three lines and the one an operator is actually looking for.
    let mut client = Client::connect(&address).unwrap();
    let refused = client.run("SELECT * FROM users;", Some(("ada", "not the one")));
    assert!(refused.is_err(), "a refused sign-in must refuse the script");
    drop(client);

    // The conversation ends on the node's own thread, so wait for the line that
    // says so rather than assuming it has already been written.
    let closed = (0..200).find_map(|_| {
        let held = lines();
        held.iter()
            .find_map(|line| connection_in(line).filter(|_| line.contains("closed")))
            .or_else(|| {
                std::thread::yield_now();
                None
            })
    });
    let id = closed.expect("the node should report the connection closing");

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
    listening();
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
