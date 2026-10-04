use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::*;
use crate::collection::{Collect, Collected, NoLog};
use crate::gathering::{Gather, Page};
use crate::grant::{Ballot, Round, Vote};
use crate::link::tests::{Authority, THERE, hello, settled, voted};
use crate::link::{Ask, Credential};
use crate::peer::Purpose;
use tessari_session::RefusalKind;
use tessari_types::{Epoch, Reach};

const HERE: [u8; NODE_ID_LEN] = [7_u8; NODE_ID_LEN];

/// A node with no log, holding what [`hello`] says, remembering whom it met.
struct Holder {
    met: std::sync::Mutex<Vec<Met>>,
    /// The build this node greets as, when not this one's.
    build: Option<tessari_encoding::NodeVersion>,
    /// How many greetings it gave — one per connection a peer opened.
    greeted: std::sync::atomic::AtomicUsize,
    /// Never moved: no test here commits, so a stream would only beat.
    commits: (
        tokio::sync::watch::Sender<u64>,
        tokio::sync::watch::Receiver<u64>,
    ),
}

impl Origin for Holder {
    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        NoLog.collected(follower, asked)
    }

    fn gathered(&self, asker: [u8; NODE_ID_LEN], asked: &Gather) -> Result<Page> {
        NoLog.gathered(asker, asked)
    }

    fn places(&self, candidate: [u8; NODE_ID_LEN], range: Reach) -> bool {
        NoLog.places(candidate, range)
    }

    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        NoLog.copied(follower, write)
    }
}

impl Holding for Holder {
    fn hello(&self) -> Result<Hello> {
        self.greeted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let said = hello(HERE);
        Ok(Hello {
            build: self.build.unwrap_or(said.build),
            ..said
        })
    }

    fn met(&self, met: &Met) {
        if let Ok(mut held) = self.met.lock() {
            held.push(*met);
        }
    }

    fn commits(&self) -> tokio::sync::watch::Receiver<u64> {
        self.commits.1.clone()
    }

    fn coordinated(
        &self,
        _: [u8; NODE_ID_LEN],
        _: &crate::assertion::Assertion,
        _: &crate::coordination::Coordinate,
    ) -> std::result::Result<tessaridb::Coordinated, String> {
        Err("this test door carries no requests".to_owned())
    }

    fn across(
        &self,
        _: [u8; NODE_ID_LEN],
        _: &crate::assertion::Assertion,
        _: &[u8],
    ) -> std::result::Result<Vec<u8>, tessari_session::PartRefused> {
        Err(tessari_session::PartRefused {
            kind: tessari_session::RefusalKind::Invalid,
            reason: "this test door writes no cross-leader records".to_owned(),
        })
    }
}

/// A door for `HERE`, served on a runtime of its own until the test ends.
struct Served {
    address: SocketAddr,
    holder: Arc<Holder>,
    stop: CancellationToken,
    serving: tokio::task::JoinHandle<Result<()>>,
    runtime: tokio::runtime::Runtime,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn served(authority: &Authority) -> Served {
    served_as(authority, None)
}

fn served_as(authority: &Authority, build: Option<tessari_encoding::NodeVersion>) -> Served {
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(HERE, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the door");
    let holder = Arc::new(Holder {
        met: std::sync::Mutex::new(Vec::new()),
        build,
        greeted: std::sync::atomic::AtomicUsize::new(0),
        commits: tokio::sync::watch::channel(0),
    });
    let stop = CancellationToken::new();
    let serving = (stop.clone(), Arc::clone(&holder));
    let serving = runtime.spawn(async move {
        let (stop, holder) = serving;
        peers
            .serve(stop, HERE, Arc::new(Deciding::holding(settled())), holder)
            .await
    });
    Served {
        address,
        holder,
        stop,
        serving,
        runtime,
    }
}

fn peer(authority: &Authority) -> Credential {
    authority.issue(THERE, Purpose::Peer)
}

#[test]
fn a_peer_is_greeted_and_its_ballot_answered_by_the_door_on_the_runtime() {
    let authority = Authority::new();
    let door = served(&authority);
    let (said, _) = crate::link::tests::call_with(
        door.address,
        peer(&authority),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    )
    .expect("a proved peer is greeted");
    assert_eq!(said.node, HERE);

    let ballot: Ballot = Round::opened(Epoch::new(5), THERE, 3).ballot();
    let (_, answered) = crate::link::tests::call_with(
        door.address,
        peer(&authority),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Ballot(&ballot),
    )
    .expect("a ballot is answered");
    assert_eq!(
        voted(&answered),
        Some(Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        })
    );
    // Recorded through the node, as the synchronous door's caller did. The
    // record is written after the answer, so it is waited for, boundedly.
    let deadline = Instant::now() + Duration::from_secs(GREETING_SECONDS);
    while door.holder.met.lock().map_or(0, |held| held.len()) < 2 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let met = door.holder.met.lock().expect("the record").clone();
    assert_eq!(met.len(), 2, "both connections were recorded");
    assert!(met.iter().any(|m| m.voted
        == Some(Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        })));
}

/// What the door answers a carried cross-leader record with, as the kind
/// of refusal the asking node is handed — the holder here refuses every
/// record as invalid (Q-924).
fn across_refused(
    (door, authority): (&Served, &Authority),
    signer: &Credential,
    link: Credential,
) -> Option<RefusalKind> {
    use crate::assertion::{Assertion, Principal, now_ms, request_digest};
    let asked = b"a record the holder refuses".to_vec();
    let carried = crate::across::Carried {
        signed: Assertion {
            from: THERE,
            to: HERE,
            principal: Principal::Anonymous,
            request: request_digest(None, None, crate::across::ACROSS, &asked),
            nonce: [9; 16],
            issued_ms: now_ms(),
            expires_ms: now_ms().saturating_add(10_000),
        }
        .sign(&signer.key)
        .expect("signed"),
        asked,
    };
    match crate::link::tests::call_with(
        door.address,
        link,
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Across(&carried),
    ) {
        Err(Error::RefusedAcross(refused)) => Some(refused.kind),
        _ => None,
    }
}

/// A record the holder refuses, signed by `signer`, under its own `nonce`.
fn record(signer: &Credential, nonce: u8) -> crate::across::Carried {
    use crate::assertion::{Assertion, Principal, now_ms, request_digest};
    let asked = b"a record the holder refuses".to_vec();
    crate::across::Carried {
        signed: Assertion {
            from: THERE,
            to: HERE,
            principal: Principal::Anonymous,
            request: request_digest(None, None, crate::across::ACROSS, &asked),
            nonce: [nonce; 16],
            issued_ms: now_ms(),
            expires_ms: now_ms().saturating_add(10_000),
        }
        .sign(&signer.key)
        .expect("signed"),
        asked,
    }
}

/// Ask the door once on a fresh link, greeting as `build`, and keep the
/// link if it was kept.
fn ask_keeping(
    (door, authority): (&Served, &Authority),
    mine: &Credential,
    build: tessari_encoding::NodeVersion,
    nonce: u8,
) -> (
    crate::across::kept::Reply,
    Option<crate::across::kept::Kept>,
) {
    let keys = crate::link::tests::keys(
        Credential {
            chain: mine.chain.clone(),
            key: mine.key.clone_key(),
        },
        &authority.der(),
    )
    .expect("keys");
    crate::across::kept::across_keeping(
        &door.address.to_string(),
        (&keys, keys.duplicate()),
        HERE,
        &Hello {
            build,
            ..hello(THERE)
        },
        (&record(mine, nonce), Duration::from_secs(GREETING_SECONDS)),
    )
    .expect("a link")
}

const KEPT: tessari_encoding::NodeVersion = crate::across::kept::KEPT_FROM;

#[test]
fn a_kept_link_carries_the_next_cross_leader_record_without_greeting_again() {
    let authority = Authority::new();
    let door = served_as(&authority, Some(KEPT));
    let mine = peer(&authority);
    let (first, kept) = ask_keeping((&door, &authority), &mine, KEPT, 1);
    assert!(
        matches!(&first, Err(refused) if refused.kind == RefusalKind::Invalid),
        "{first:?}"
    );
    let mut kept = kept.expect("both ends greeted at the build that keeps links");
    for nonce in [2, 3] {
        let next = crate::across::kept::across_on(&mut kept, &record(&mine, nonce));
        assert!(
            matches!(&next, Ok(Err(refused)) if refused.kind == RefusalKind::Invalid),
            "{next:?}"
        );
    }
    assert_eq!(
        door.holder
            .greeted
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "three records, one greeting"
    );
    // A record replayed on the kept link is disbelieved as on any other.
    let replayed = crate::across::kept::across_on(&mut kept, &record(&mine, 3));
    assert!(
        matches!(&replayed, Ok(Err(refused)) if refused.kind == RefusalKind::Forbidden),
        "{replayed:?}"
    );
}

#[test]
fn a_link_is_kept_only_when_both_ends_greet_at_the_build_that_keeps_it() {
    let authority = Authority::new();
    let older = tessari_encoding::NodeVersion {
        major: 0,
        minor: 24,
        patch: 0,
    };
    let mine = peer(&authority);
    // A door on the older build answers one record a connection.
    let door = served_as(&authority, Some(older));
    assert!(ask_keeping((&door, &authority), &mine, KEPT, 1).1.is_none());
    // A coordinator on the older build is not kept for, either.
    let door = served_as(&authority, Some(KEPT));
    assert!(
        ask_keeping((&door, &authority), &mine, older, 2)
            .1
            .is_none()
    );
}

#[test]
fn a_refused_cross_leader_record_comes_back_with_the_kind_of_its_refusal() {
    let authority = Authority::new();
    let door = served(&authority);
    let mine = peer(&authority);
    let same = |credential: &Credential| Credential {
        chain: credential.chain.clone(),
        key: credential.key.clone_key(),
    };
    // Believed, and refused by the node holding the range: its own kind.
    assert_eq!(
        across_refused((&door, &authority), &mine, same(&mine)),
        Some(RefusalKind::Invalid)
    );
    // Signed by a key the connection did not prove: the door will not act
    // for that caller, and asking again changes nothing.
    assert_eq!(
        across_refused((&door, &authority), &peer(&authority), same(&mine)),
        Some(RefusalKind::Forbidden)
    );
}

#[test]
fn a_caller_that_says_nothing_does_not_hold_the_door() {
    let authority = Authority::new();
    let door = served(&authority);
    // A socket that never starts its handshake: a stranger, or a peer that
    // died after connecting. The synchronous door sat on it for the whole
    // greeting deadline before it could take anyone else.
    let _quiet = TcpStream::connect(door.address).expect("a quiet connection");
    let began = Instant::now();
    let (said, _) = crate::link::tests::call_with(
        door.address,
        peer(&authority),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    )
    .expect("a proved peer is greeted while a stranger holds a socket");
    let waited = began.elapsed();
    assert_eq!(said.node, HERE);
    assert!(
        waited < Duration::from_secs(GREETING_SECONDS / 2),
        "the second peer waited {waited:?} behind the quiet one"
    );
}

#[test]
fn a_caller_offering_no_credential_is_refused_inside_the_handshake() {
    let authority = Authority::new();
    let door = served(&authority);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(authority.der()).expect("the test authority");
    let settings = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(credential::names(HERE, Purpose::Peer))
        .expect("the door's own name");
    let mut session =
        rustls::ClientConnection::new(Arc::new(settings), name).expect("a client session");
    let mut socket = TcpStream::connect(door.address).expect("a connection");
    socket
        .set_read_timeout(Some(Duration::from_secs(GREETING_SECONDS)))
        .expect("a read deadline");
    let mut link = rustls::Stream::new(&mut session, &mut socket);
    // The greeting a peer would send, if the door let it get that far.
    let sent = std::io::Write::write_all(&mut link, &hello(THERE).encode());
    let mut answer = [0_u8; 1];
    let heard = std::io::Read::read(&mut link, &mut answer);
    assert!(
        sent.is_err() || heard.is_err() || heard.is_ok_and(|read| read == 0),
        "a connection that proved nothing must not be answered"
    );
    // Nobody reached the node: nothing was recorded.
    assert!(door.holder.met.lock().expect("the record").is_empty());
}

#[test]
fn a_stopped_door_returns_and_takes_nobody_new() {
    let authority = Authority::new();
    let mut door = served(&authority);
    door.stop.cancel();
    let serving = &mut door.serving;
    let returned = door.runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(GREETING_SECONDS), serving).await
    });
    assert!(
        matches!(returned, Ok(Ok(Ok(())))),
        "a stop ends the door cleanly: {returned:?}"
    );
    let refused = crate::link::tests::call_with(
        door.address,
        peer(&authority),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Nothing,
    );
    assert!(refused.is_err(), "a stopped door greets nobody");
}
