//! The signing rows: a store user signs in over the wire exactly as a store
//! user signs in anywhere — the challenge, the password, the refusal naming
//! the reason, and the grants of the signed-in user binding the session.

use super::{Client, Seen, refused, rows, serving_with};
use tokio::io::AsyncReadExt as _;

const SETUP: &str = "DEFINE TABLE orders SCHEMALESS; CREATE orders:1 = { band: 1 }; \
     DEFINE USER app_owner ROLE owner PASSWORD 'owner-secret';";

#[tokio::test(flavor = "multi_thread")]
async fn a_right_password_signs_in_and_reads() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client
        .start(Some("app_owner"), "app.main", Some("owner-secret"))
        .await;
    assert!(seen.contains(&Seen::Auth(0)), "AuthenticationOk: {seen:?}");
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    let seen = client.query("SELECT id FROM orders").await;
    assert_eq!(rows(&seen).len(), 1);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_refuses_with_the_stores_words_and_closes() {
    let (address, stop, _) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client
        .start(Some("app_owner"), "app.main", Some("not-it"))
        .await;
    let (code, message) = refused(&seen).expect("the sign-in refusal");
    assert_eq!(code, "28P01", "{seen:?}");
    assert!(!message.is_empty());
    // The protocol expects the connection to close after a startup refusal.
    let mut at = [0_u8; 1];
    let got = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        client.stream.read(&mut at),
    )
    .await;
    assert!(matches!(got, Ok(Ok(0))), "the connection closes: {got:?}");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusing_role_is_refused_with_its_class() {
    let (address, stop, db) = serving_with(SETUP).await;
    let mut owner = db.session();
    owner.sign_in("app_owner", "owner-secret").await.unwrap();
    owner
        .run(
            "DEFINE USER app_reader ON app.main ROLE viewer PASSWORD 'reader-secret'; \
              DEFINE USER nobody ON app.main ROLE viewer PASSWORD 'x';",
        )
        .await
        .unwrap();
    drop(owner);

    // The reader signs in and reads what its role reaches.
    let mut client = Client::to(&address).await;
    let seen = client
        .start(Some("app_reader"), "app.main", Some("reader-secret"))
        .await;
    assert!(seen.contains(&Seen::Auth(0)), "{seen:?}");

    // A user of one tenancy, asking work of another, is refused with the
    // tenancy refusal at the first statement — the sign-in itself is global.
    let mut other = Client::to(&address).await;
    let seen = other
        .start(Some("app_reader"), "other.main", Some("reader-secret"))
        .await;
    assert!(seen.contains(&Seen::Auth(0)), "{seen:?}");
    let seen = other.query("SELECT id FROM orders").await;
    let (code, _) = refused(&seen).expect("the tenancy refusal: {seen:?}");
    assert_eq!(code, "42501", "{seen:?}");
    stop.cancel();
}
