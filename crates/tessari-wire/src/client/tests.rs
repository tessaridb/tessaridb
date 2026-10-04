#![allow(clippy::panic)]

use std::io::{BufReader, BufWriter};
use std::net::TcpListener;
use std::thread;

use tessari_encoding::NODE_ID_LEN;
use tessari_types::Epoch;

use super::*;
use crate::redirect::Settlement;

/// A node that greets, reads one frame, and answers with the tag it was
/// given — the smallest thing a real `Client` will talk to over a real
/// socket, which is the only way to exercise `run_routed`'s reader.
///
/// Bound to port zero rather than a number: the two suites that use fixed
/// ports have to run alone, and this one has no reason to join them.
fn node_answering(kind: frame::Kind, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("the port it took").to_string();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the client this test dials");
        let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
        let mut writer = BufWriter::new(stream);
        let mut both = frame::Duplex {
            reader: &mut reader,
            writer: &mut writer,
        };
        frame::greet(&mut both).expect("a greeting from a client this build wrote");
        frame::read(&mut reader)
            .expect("the request")
            .expect("a request");
        frame::write(&mut writer, kind, &body).expect("the answer this test exists to send");
    });
    address
}

/// A node that answers a **sequence** of frames, one per request.
///
/// The single-answer helper above cannot express this wave's subject at all:
/// staleness is a relation between two redirects, so a client that has seen
/// one is the only client the check applies to.
fn node_answering_in_turn(frames: Vec<(frame::Kind, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("the port it took").to_string();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the client this test dials");
        let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
        let mut writer = BufWriter::new(stream);
        {
            let mut both = frame::Duplex {
                reader: &mut reader,
                writer: &mut writer,
            };
            frame::greet(&mut both).expect("a greeting from a client this build wrote");
        }
        for (kind, body) in frames {
            frame::read(&mut reader)
                .expect("the request")
                .expect("a request");
            frame::write(&mut writer, kind, &body).expect("the answer in its turn");
        }
    });
    address
}

/// A redirect decided under `epoch`, at an address that names it.
fn a_redirect_under(epoch: u64) -> Elsewhere {
    Elsewhere {
        endpoint: format!("10.0.0.9:{}", 9080_u64.saturating_add(epoch)),
        node: [3; NODE_ID_LEN],
        epoch: Epoch::new(epoch),
        settlement: Settlement::Transient,
    }
}

#[test]
fn a_redirect_older_than_one_already_taken_is_refused_and_names_the_newer() {
    // The criterion's own scenario: routed under a leadership, told about a
    // newer one, then handed the old decision again. Undated, the client
    // would follow it back into an arrangement it has already left and could
    // not tell that from progress.
    let address = node_answering_in_turn(vec![
        (frame::Kind::Elsewhere, a_redirect_under(5).encode()),
        (frame::Kind::Elsewhere, a_redirect_under(4).encode()),
    ]);
    let mut client = Client::connect(&address).expect("a node this test started");

    client
        .run_routed("SELECT 1;", None, &Parameters::new())
        .expect("the first redirect is news to a client that has been told nothing");

    let refused = client
        .run_routed("SELECT 1;", None, &Parameters::new())
        .expect_err("a decision this client has already moved past is not an instruction");

    let Error::StaleRedirect { named, held } = refused else {
        panic!("refused for the wrong reason: {refused}");
    };
    assert_eq!(named, Epoch::new(4));
    // **Naming the newer one is the requirement**, not merely failing: a
    // caller told only *no* learns nothing about why, while a caller told
    // which leadership is current knows it is behind and by how much. Raft
    // returns its own term for the same reason.
    assert_eq!(held, Epoch::new(5));
}

#[test]
fn the_refusal_carries_both_leaderships_in_its_own_words() {
    let address = node_answering_in_turn(vec![
        (frame::Kind::Elsewhere, a_redirect_under(9).encode()),
        (frame::Kind::Elsewhere, a_redirect_under(2).encode()),
    ]);
    let mut client = Client::connect(&address).expect("a node this test started");
    client
        .run_routed("SELECT 1;", None, &Parameters::new())
        .expect("the first");
    let said = client
        .run_routed("SELECT 1;", None, &Parameters::new())
        .expect_err("the replay")
        .to_string();

    assert!(
        said.contains('9'),
        "the refusal hid the current leadership: {said}"
    );
    assert!(
        said.contains('2'),
        "the refusal hid the one it refused: {said}"
    );
}

#[test]
fn two_redirects_under_one_leadership_both_route() {
    // The ordinary two-hop route — sent to one node, and that node sending
    // this caller to a second, both deciding under the same leadership.
    // Refusing equality would break it, which is why the comparison is
    // strictly less-than and not less-or-equal.
    let address = node_answering_in_turn(vec![
        (frame::Kind::Elsewhere, a_redirect_under(7).encode()),
        (frame::Kind::Elsewhere, a_redirect_under(7).encode()),
    ]);
    let mut client = Client::connect(&address).expect("a node this test started");

    for hop in 1..=2 {
        let served = client
            .run_routed("SELECT 1;", None, &Parameters::new())
            .unwrap_or_else(|why| panic!("hop {hop} of a same-epoch route was refused: {why}"));
        assert!(matches!(served, Served::Elsewhere(_)));
    }
}

fn a_redirect() -> Elsewhere {
    Elsewhere {
        endpoint: "10.0.0.9:9080".to_owned(),
        node: [3; NODE_ID_LEN],
        epoch: Epoch::new(41),
        settlement: Settlement::Settled,
    }
}

#[test]
fn a_redirect_is_an_answer_to_run_routed_and_not_an_error() {
    let sent = a_redirect();
    let address = node_answering(frame::Kind::Elsewhere, sent.encode());
    let mut client = Client::connect(&address).expect("a node this test started");
    let served = client
        .run_routed("SELECT 1;", None, &Parameters::new())
        .expect("a redirect is not a failure");
    assert_eq!(served, Served::Elsewhere(sent));
}

#[test]
fn a_caller_that_cannot_follow_a_redirect_is_told_where_the_read_belonged() {
    // The point of the wrapper: `run_with` must fail — its contract is
    // *give me the answers* — but the failure has to name the endpoint
    // rather than report tag 13 as an unknown frame, which is what it said
    // before this wave.
    let address = node_answering(frame::Kind::Elsewhere, a_redirect().encode());
    let mut client = Client::connect(&address).expect("a node this test started");
    let held = client
        .run_with("SELECT 1;", None, &Parameters::new())
        .expect_err("a caller asking for answers cannot follow an instruction");
    assert!(
        matches!(held, Error::Redirected { ref endpoint, .. } if endpoint == "10.0.0.9:9080"),
        "the refusal said {held:?} instead of naming where the read belonged"
    );
}

#[test]
fn a_subscriber_still_refuses_a_redirect() {
    // A subscription is a position in ONE node's log, so there is nothing
    // for another node to answer and this refusal outlives the delivery
    // work — it is not a placeholder.
    let address = node_answering(frame::Kind::Elsewhere, a_redirect().encode());
    let client = Client::connect(&address).expect("a node this test started");
    let mut feed = client
        .follow(&Follow {
            from: 0,
            table: None,
            cursor: None,
        })
        .expect("the subscription this test sends");
    assert!(matches!(feed.wait(), Err(Error::UnknownFrame { tag: 13 })));
}
