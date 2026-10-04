use super::*;

const ME: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const PEER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

/// A peer credential: the certificate a handshake would prove, and its key.
fn credential() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let key = rcgen::KeyPair::generate().expect("a key");
    let params = rcgen::CertificateParams::new(vec![crate::credential::names(
        PEER,
        crate::peer::Purpose::Peer,
    )])
    .expect("params");
    let certificate = params.self_signed(&key).expect("a certificate");
    (
        certificate.der().clone(),
        PrivateKeyDer::try_from(key.serialize_der()).expect("a key in DER"),
    )
}

fn request() -> [u8; 32] {
    request_digest(Some("prod"), Some("work"), "CREATE t:1;", &[])
}

fn made(now: u64) -> Assertion {
    Assertion {
        from: PEER,
        to: ME,
        principal: Principal::User {
            id: 7,
            account: [9; 32],
        },
        request: request(),
        nonce: [5; 16],
        issued_ms: now,
        expires_ms: now.saturating_add(10_000),
    }
}

fn judged(
    signed: &Signed,
    shown: &CertificateDer<'_>,
    now: u64,
    replays: &Replays,
) -> std::result::Result<Assertion, Disbelieved> {
    signed
        .verify(shown, PEER, ME, (request(), now), replays)
        .copied()
}

#[test]
fn a_signed_assertion_crosses_the_wire_and_is_believed_once() {
    let (shown, key) = credential();
    let now = now_ms();
    let signed = made(now).sign(&key).expect("signed");
    let (heard, end) = Signed::decode(&signed.encode(), 0).expect("decoded");
    assert_eq!(end, signed.encode().len());
    assert_eq!(heard, signed);
    let replays = Replays::default();
    assert_eq!(judged(&heard, &shown, now, &replays), Ok(made(now)));
    assert_eq!(
        judged(&heard, &shown, now, &replays),
        Err(Disbelieved::Replayed),
        "the same assertion was believed twice"
    );
}

#[test]
fn an_assertion_signed_by_another_key_is_refused() {
    let (shown, _) = credential();
    let (_, forger) = credential();
    let now = now_ms();
    let forged = made(now).sign(&forger).expect("signed");
    assert_eq!(
        judged(&forged, &shown, now, &Replays::default()),
        Err(Disbelieved::BadSignature)
    );
}

#[test]
fn an_assertion_altered_after_signing_is_refused() {
    let (shown, key) = credential();
    let now = now_ms();
    let signed = made(now).sign(&key).expect("signed");
    // Every field, one at a time, so no field is left out of what is signed.
    let alterations: [fn(&mut Assertion); 5] = [
        |a| {
            a.principal = Principal::User {
                id: 8,
                account: [9; 32],
            }
        },
        |a| a.principal = Principal::Anonymous,
        |a| a.nonce = [6; 16],
        |a| a.issued_ms = a.issued_ms.saturating_sub(1),
        |a| a.expires_ms = a.expires_ms.saturating_add(1),
    ];
    for (index, alter) in alterations.iter().enumerate() {
        let mut altered = signed.clone();
        alter(&mut altered.assertion);
        assert_eq!(
            judged(&altered, &shown, now, &Replays::default()),
            Err(Disbelieved::BadSignature),
            "alteration {index} was believed"
        );
    }
}

#[test]
fn an_assertion_for_another_request_or_node_or_from_another_peer_is_refused() {
    let (shown, key) = credential();
    let now = now_ms();
    let replays = Replays::default();
    let mut other = made(now);
    other.request = request_digest(Some("prod"), Some("work"), "DELETE t:1;", &[]);
    let other = other.sign(&key).expect("signed");
    assert_eq!(
        judged(&other, &shown, now, &replays),
        Err(Disbelieved::RequestAltered)
    );
    let mut elsewhere = made(now);
    elsewhere.to = [3; NODE_ID_LEN];
    let elsewhere = elsewhere.sign(&key).expect("signed");
    assert_eq!(
        judged(&elsewhere, &shown, now, &replays),
        Err(Disbelieved::NotForThisNode)
    );
    let mut impostor = made(now);
    impostor.from = [4; NODE_ID_LEN];
    let impostor = impostor.sign(&key).expect("signed");
    assert_eq!(
        judged(&impostor, &shown, now, &replays),
        Err(Disbelieved::NotTheSigner)
    );
}

#[test]
fn an_assertion_outside_its_life_is_refused() {
    let (shown, key) = credential();
    let now = now_ms();
    let signed = made(now).sign(&key).expect("signed");
    assert_eq!(
        judged(
            &signed,
            &shown,
            now.saturating_add(10_001),
            &Replays::default()
        ),
        Err(Disbelieved::Expired)
    );
    assert_eq!(
        judged(
            &signed,
            &shown,
            now.saturating_sub(CLOCK_SKEW_MILLIS).saturating_sub(1),
            &Replays::default()
        ),
        Err(Disbelieved::NotYetValid)
    );
    let mut long = made(now);
    long.expires_ms = now.saturating_add(LONGEST_LIFE_MILLIS).saturating_add(1);
    let long = long.sign(&key).expect("signed");
    assert_eq!(
        judged(&long, &shown, now, &Replays::default()),
        Err(Disbelieved::TooLong)
    );
}

#[test]
fn a_refused_assertion_does_not_spend_its_nonce() {
    // Otherwise a forger could burn a real assertion's nonce in advance.
    let (shown, key) = credential();
    let (_, forger) = credential();
    let now = now_ms();
    let replays = Replays::default();
    let forged = made(now).sign(&forger).expect("signed");
    assert!(judged(&forged, &shown, now, &replays).is_err());
    let real = made(now).sign(&key).expect("signed");
    assert_eq!(judged(&real, &shown, now, &replays), Ok(made(now)));
}

#[test]
fn a_full_replay_table_refuses_rather_than_forgets() {
    let replays = Replays::default();
    let far = now_ms().saturating_add(60_000);
    for index in 0..REPLAY_TABLE {
        let mut nonce = [0_u8; 16];
        nonce[..8].copy_from_slice(&u64::try_from(index).expect("small").to_be_bytes());
        replays.remember(nonce, far, 0).expect("room");
    }
    assert_eq!(
        replays.remember([0xff; 16], far, 0),
        Err(Disbelieved::ReplayTableFull)
    );
    // Once their lives are over the table has room again.
    assert_eq!(
        replays.remember([0xff; 16], far, far.saturating_add(1)),
        Ok(())
    );
}
