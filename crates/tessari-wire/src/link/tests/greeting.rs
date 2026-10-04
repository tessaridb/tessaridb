use super::*;

#[test]
fn two_nodes_that_prove_who_they_are_exchange_what_they_hold() {
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let listening = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
    });

    let theirs = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    )
    .expect("a peer that proved itself is answered")
    .0;

    let heard = listening
        .join()
        .expect("the door's thread")
        .expect("the door admits a peer credential naming the greeter");
    // Each end learned the other's facts, and neither learned them from a
    // certificate: the epoch and the tail are in the frame because a
    // credential outlives both.
    assert_eq!(heard.said.node, THERE);
    assert_eq!(heard.said.epoch, Epoch::new(4));
    assert_eq!(heard.said.tail, Sequence::new(9));
    assert_eq!(heard.voted, None, "nobody asked for anything");
    assert_eq!(theirs.node, HERE);
}

#[test]
fn the_greeting_carries_what_this_node_holds_when_the_peer_arrives_not_when_the_door_opened() {
    let authority = Authority::new();
    let (peers, _) = door(&authority);
    let address = peers.address().expect("the door's address");

    // The node's log tail, which moves while the door is waiting. A `Hello`
    // is entirely a claim about state, and this is the field a router reads
    // beside the copy's age — so *when* it was read is the whole criterion.
    let tail = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(9));
    let read = std::sync::Arc::clone(&tail);
    let (entered, waiting) = std::sync::mpsc::channel();

    let listening = std::thread::spawn(move || {
        // Sent before `greet`, so the advance below cannot land while this
        // thread is still being scheduled.
        entered.send(()).expect("the test is still listening");
        peers.greet(
            || {
                Ok(Hello::about(
                    &identity(HERE),
                    Epoch::new(4),
                    Sequence::new(read.load(std::sync::atomic::Ordering::SeqCst)),
                    LEVEL.leadership,
                    Some(core::time::Duration::ZERO),
                    None,
                ))
            },
            &HERE,
            &Deciding::holding(settled()),
            &NoLog,
        )
    });
    waiting.recv().expect("the door's thread starts");
    // The pause is for the FALSIFICATION and not for this assertion. Reading
    // on arrival is correct whatever the timing, because the closure cannot
    // run until `accept` returns and `accept` cannot return until the
    // connection below is made. Restore the eager read and the door has
    // microseconds in which to take the stale value — this widens that
    // window so the arm bites every run instead of most of them.
    std::thread::sleep(core::time::Duration::from_millis(100));
    tail.store(41, std::sync::atomic::Ordering::SeqCst);

    let theirs = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    )
    .expect("a peer that proved itself is answered")
    .0;

    listening
        .join()
        .expect("the door's thread")
        .expect("the door admits a peer credential naming the greeter");
    // 41 and not 9: the door greeted with what this node held when the peer
    // arrived, not with what it held an idle stretch earlier.
    assert_eq!(
        theirs.tail,
        Sequence::new(41),
        "the greeting carries the tail read on arrival, not the one read when the door opened"
    );
}

#[test]
fn a_door_with_no_log_refuses_a_collection_as_a_refusal_and_not_by_hanging_up() {
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let listening = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
    });

    let failure = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Records(Collect {
            home: tessari_types::Reach::Store,
            from: Sequence::new(7),
            limit: 16,
        }),
    )
    .expect_err("a door with no log behind it has nothing to hand over");

    // The position it asked from, handed back. That is what tells the
    // follower *not from here* apart from *you are level*, and it is the
    // whole reason this is `Uncollectable` and not a dropped socket: a
    // conversation that ends mid-frame reaches whoever reads it as a network
    // fault and sends them to a packet capture.
    assert!(
        matches!(failure, Error::Uncollectable { from: 7 }),
        "{failure}"
    );
    // And the door itself finished the conversation rather than failing:
    // it served the greeting, refused the ask, and closed in order.
    let heard = listening
        .join()
        .expect("the door's thread")
        .expect("a refused collection is a served connection, not a failed one");
    assert_eq!(heard.said.node, THERE);
    assert_eq!(heard.voted, None, "a collection is not a vote");
}

#[test]
fn a_client_credential_on_the_peer_link_is_refused_on_its_purpose() {
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let listening = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
    });

    // The id is perfectly correct. What is wrong is the link it was issued
    // for, which is the criterion's own sentence.
    drop(call_with(
        address,
        authority.issue(THERE, Purpose::Client),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    ));

    let refused = listening
        .join()
        .expect("the door's thread")
        .expect_err("a client credential is not a peer credential");
    assert!(matches!(refused, Error::NotAPeerCredential), "{refused}");
}

#[test]
fn a_credential_that_does_not_name_the_greeter_is_refused_and_names_the_file() {
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let listening = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
    });

    // Issued by the right authority, for the right link, for the wrong node.
    drop(call_with(
        address,
        authority.issue([3_u8; NODE_ID_LEN], Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    ));

    let refused = listening
        .join()
        .expect("the door's thread")
        .expect_err("a credential that names another node is refused");
    let said = refused.to_string();
    // The refusal is read by a person, so it is the rendering that is
    // asserted: it must carry a fingerprint, because that is the only thing
    // here that identifies one file on one machine.
    assert!(
        matches!(refused, Error::CredentialNamesAnother { .. }),
        "{said}"
    );
    assert!(said.contains("sha256 "), "{said}");
}

#[test]
fn a_connection_offering_no_credential_never_reaches_a_frame() {
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let listening = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
    });

    let mut roots = rustls::RootCertStore::empty();
    roots.add(authority.der()).expect("the test authority");
    let settings = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(names(HERE, Purpose::Peer))
        .expect("the door's own name");
    let mut session =
        rustls::ClientConnection::new(Arc::new(settings), name).expect("a client session");
    if let Ok(mut socket) = TcpStream::connect(address) {
        let mut link = rustls::Stream::new(&mut session, &mut socket);
        drop(std::io::Write::write_all(&mut link, b"never read"));
    }

    let refused = listening
        .join()
        .expect("the door's thread")
        .expect_err("a connection that proves nothing is refused");
    // Refused by the transport, inside the handshake — the earliest place a
    // refusal can happen, and before a single frame was parsed.
    assert!(matches!(refused, Error::Transport(_)), "{refused}");
}

/// Fill the queue of a listener that never accepts, so the next connect
/// gets no answer at all — the black-holed peer, without leaving loopback.
///
/// A full accept queue drops a SYN rather than refusing it (measured on
/// macOS: the 129th connect to a never-accepting listener times out).
fn a_peer_that_answers_no_syn() -> (std::net::TcpListener, Vec<TcpStream>, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("its address");
    let mut held = Vec::new();
    while held.len() < 1024 {
        match TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
            Ok(stream) => held.push(stream),
            Err(_) => break,
        }
    }
    assert!(
        held.len() < 1024,
        "a thousand connects to a listener that never accepts all completed"
    );
    (listener, held, address)
}

#[test]
fn a_peer_that_never_answers_the_connect_costs_the_deadline_and_no_more() {
    let (_listener, _held, address) = a_peer_that_answers_no_syn();
    // A deadline shorter than the operating system's own: this loopback
    // gives up on its own after about eight seconds, a remote black hole
    // only after the kernel's connect limit (`keepinit`, 75 s here).
    let bound = Duration::from_secs(1);
    let began = std::time::Instant::now();
    let failed = super::super::dial::connect(address, bound);
    let waited = began.elapsed();
    assert!(failed.is_err(), "nobody answered, so nothing connected");
    assert!(
        waited < bound.saturating_mul(3),
        "a black-holed peer held the connect for {waited:?}; the deadline is {bound:?}"
    );
}
