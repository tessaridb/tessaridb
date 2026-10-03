use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tessari_encoding::{TRANSACTION_ID_LEN, TransactionId};
use tessari_session::AcrossAsk;
use tessari_storage::RecordAddress;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::*;
use crate::assertion::{Disbelieved, Replays};

const ME: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const PEER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

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

fn resolving(committed: bool) -> Vec<u8> {
    AcrossAsk::Resolve {
        transaction: TransactionId::new([4; TRANSACTION_ID_LEN]),
        committed,
        records: vec![RecordAddress::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            RecordId::Int(1),
        )],
        participants: Vec::new(),
    }
    .encode()
}

fn carried(key: &PrivateKeyDer<'_>, asked: Vec<u8>) -> Carried {
    let now = now_ms();
    Carried {
        signed: Assertion {
            from: PEER,
            to: ME,
            principal: Principal::Anonymous,
            request: request_digest(None, None, ACROSS, &asked),
            nonce: [6; 16],
            issued_ms: now,
            expires_ms: now.saturating_add(10_000),
        }
        .sign(key)
        .expect("signed"),
        asked,
    }
}

#[test]
fn a_carried_record_crosses_the_wire_and_is_believed() {
    let (shown, key) = credential();
    let sent = carried(&key, resolving(true));
    let heard = Carried::decode(&sent.encode()).expect("decoded");
    assert_eq!(heard, sent);
    let believed = heard.signed.verify(
        &shown,
        PEER,
        ME,
        (heard.digest(), now_ms()),
        &Replays::default(),
    );
    assert!(believed.is_ok(), "{believed:?}");
}

#[test]
fn a_record_swapped_under_its_assertion_is_disbelieved() {
    // The assertion was made for a committed resolution; the bytes now say
    // aborted. The digest the door checks is of the bytes that arrived.
    let (shown, key) = credential();
    let mut sent = carried(&key, resolving(true));
    sent.asked = resolving(false);
    let heard = Carried::decode(&sent.encode()).expect("decoded");
    assert_eq!(
        heard
            .signed
            .verify(
                &shown,
                PEER,
                ME,
                (heard.digest(), now_ms()),
                &Replays::default()
            )
            .copied(),
        Err(Disbelieved::RequestAltered)
    );
}

#[test]
fn trailing_bytes_are_refused() {
    let (_, key) = credential();
    let mut body = carried(&key, resolving(true)).encode();
    body.push(0);
    assert!(matches!(Carried::decode(&body), Err(Error::Malformed)));
}
