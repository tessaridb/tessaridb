//! `DEFINE AI PROVIDER` (G102 C8 sub-wave A): the provider as a catalog
//! object whose key is named as a vault secret and never written — and
//! `ai::generate` (sub-wave B) against a stub provider: one HTTPS call, the
//! sealed key in its one header, the budget spent before the wire.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_session::{Outcome, RunNow, Session};
use tessari_types::Value;

use super::materialized_views::store;

fn ready(store: &tessari_storage::Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             UNSEAL VAULT WITH 'an operator passphrase';\n\
             DEFINE VAULT secrets;",
        )
        .unwrap();
    session
}

const DECLARE: &str = "DEFINE AI PROVIDER anthropic \
    URL 'https://api.example.test/v1/messages' MODEL 'a-model' \
    KEY VAULT secrets FIELD anthropic_key BUDGET 1000 PER 1h TIMEOUT 10s;";

#[test]
fn a_provider_declares_and_drops() {
    let store = store();
    let mut session = ready(&store);
    session.run_now(DECLARE).unwrap();
    // A repeat declaration refuses by name; IF NOT EXISTS accepts it.
    assert!(
        session.run_now(DECLARE).is_err(),
        "a repeat declaration was accepted"
    );
    session
        .run_now(&DECLARE.replace("DEFINE AI PROVIDER", "DEFINE AI PROVIDER IF NOT EXISTS"))
        .unwrap();
    session.run_now("DROP AI PROVIDER anthropic;").unwrap();
    assert!(
        session.run_now("DROP AI PROVIDER anthropic;").is_err(),
        "dropping what is gone was accepted"
    );
}

#[test]
fn the_sealing_rules_hold_at_declare_time() {
    let store = store();
    let mut session = ready(&store);
    // A vault the database does not hold: the key has nowhere sealed to
    // live, so the provider refuses to exist.
    let refused = session
        .run_now(
            "DEFINE AI PROVIDER ghost URL 'https://api.example.test/v1' MODEL 'm' \
             KEY VAULT nowhere FIELD key BUDGET 10 PER 1h TIMEOUT 5s;",
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("no sealed key"),
        "the refusal did not name the sealing: {refused}"
    );
    // Plaintext never carries a key.
    assert!(
        session
            .run_now(
                "DEFINE AI PROVIDER plain URL 'http://api.example.test/v1' MODEL 'm' \
                 KEY VAULT secrets FIELD key BUDGET 10 PER 1h TIMEOUT 5s;",
            )
            .is_err(),
        "a plaintext endpoint was accepted"
    );
    // Every clause is required: TIMEOUT names no default.
    assert!(
        session
            .run_now(
                "DEFINE AI PROVIDER bare URL 'https://api.example.test/v1' MODEL 'm' \
                 KEY VAULT secrets FIELD key BUDGET 10 PER 1h;",
            )
            .is_err(),
        "a provider with no TIMEOUT was accepted"
    );
}

// ── C8-B: ai::generate against a stub provider ──────────────────────────────

fn value(session: &mut Session<'_>, statement: &str) -> Value {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

struct Stub {
    port: u16,
    roots: Arc<rustls::RootCertStore>,
}

/// A one-endpoint HTTPS provider with a certificate of its own authority.
///
/// The server runs on a thread of its own because `run_now` drives the
/// session's runtime on the calling thread, and a server sharing it could
/// never be polled while the call that reaches it is in flight. It answers
/// `connections` calls, holds the sealed key to the one header that may
/// carry it, and holds every request to `expects` — a needle the request
/// body must carry, which is how a test pins the prompt a call built. A
/// request missing either fails the test inside the server.
fn stub(
    status: &'static str,
    body: &'static str,
    expects: &'static str,
    connections: usize,
) -> Stub {
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let (port_tx, port_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            for _ in 0..connections {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(tcp).await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.contains("authorization: Bearer sk-stub-key"),
                    "the sealed key did not reach the provider: {request}"
                );
                assert!(
                    request.contains(expects),
                    "the request did not carry `{expects}`: {request}"
                );
                let answer = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                use tokio::io::AsyncWriteExt;
                stream.write_all(answer.as_bytes()).await.unwrap();
                // `shutdown` sends the TLS close_notify; a bare drop would
                // close the TCP stream under the client's feet.
                stream.shutdown().await.unwrap();
            }
        });
    });
    Stub {
        port: port_rx.recv().unwrap(),
        roots: Arc::new(roots),
    }
}

async fn read_request(
    stream: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> String {
    use tokio::io::AsyncReadExt;
    let mut received = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the caller closed before the request was whole");
        received.extend_from_slice(buffer.get(..read).unwrap_or_default());
        if let Some(at) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break at.saturating_add(4);
        }
    };
    let head = String::from_utf8_lossy(received.get(..head_end).unwrap_or_default()).to_string();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|held| held.trim().parse().ok())
        .unwrap_or(0);
    while received.len() < head_end.saturating_add(length) {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the caller closed inside the body");
        received.extend_from_slice(buffer.get(..read).unwrap_or_default());
    }
    String::from_utf8_lossy(&received).to_string()
}

/// The vault holds the key, the provider names it, and the session trusts
/// the stub's own authority in place of the public roots.
fn declared<'a>(store: &'a tessari_storage::Store, stub: &Stub, budget: &str) -> Session<'a> {
    let mut session = ready(store).trusting_ai_roots(stub.roots.clone());
    session
        .run_now(&format!(
            "DEFINE FIELD token ON secrets TYPE string SECRET;
             CREATE secrets:'anthropic_key' = {{ token: 'sk-stub-key' }};
             DEFINE AI PROVIDER stub URL 'https://localhost:{}/v1/messages' MODEL 'a-model' \
                 KEY VAULT secrets FIELD anthropic_key BUDGET {budget} PER 1h TIMEOUT 10s;",
            stub.port,
        ))
        .unwrap();
    session
}

#[test]
fn a_generate_call_asks_the_provider_and_answers_its_text() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"stub says hello\"}}]}",
        "say hello",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(&mut session, "RETURN ai::generate('say hello');");
    assert_eq!(answer, Value::String("stub says hello".to_owned()));
}

#[test]
fn a_call_beyond_the_budget_refuses_without_reaching_the_provider() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"one\"}}]}",
        "first",
        1,
    );
    let mut session = declared(&store, &stub, "1");
    session.run_now("RETURN ai::generate('first');").unwrap();
    // The stub serves exactly one call: a second that reached the wire
    // would hang, so the named refusal is also the proof it never left.
    let refused = session
        .run_now("RETURN ai::generate('second');")
        .unwrap_err();
    assert!(
        refused.to_string().contains("spent its budget"),
        "the refusal did not name the budget: {refused}"
    );
}

#[test]
fn a_provider_refusal_answers_with_the_class_and_never_the_body() {
    let store = store();
    let stub = stub("500 Internal Server Error", "an error page", "hello", 1);
    let mut session = declared(&store, &stub, "1000");
    let refused = session
        .run_now("RETURN ai::generate('hello');")
        .unwrap_err();
    let text = refused.to_string();
    assert!(
        text.contains("refused the call (5xx)"),
        "the refusal did not carry the status class: {text}"
    );
    // The provider's page can echo the request, and the request carried the
    // key — so the refusal carries the class and nothing of the body.
    assert!(
        !text.contains("an error page"),
        "the refusal leaked the provider's body: {text}"
    );
}

#[test]
fn a_call_with_no_provider_declared_refuses_by_name() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run_now("RETURN ai::generate('hello');")
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("no AI provider answers this call"),
        "the refusal did not name the missing provider: {refused}"
    );
}

// ── C8-C: the derived four, each a prompt shape over the same call ──────────

#[test]
fn a_summarize_call_carries_its_instruction_and_answers_the_reply() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"a short summary\"}}]}",
        "Summarize this text in a few sentences",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(&mut session, "RETURN ai::summarize('a very long text');");
    assert_eq!(answer, Value::String("a short summary".to_owned()));
}

#[test]
fn an_extract_call_names_the_field_and_answers_the_reply() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"bob@example.test\"}}]}",
        // The prompt's quotes ride JSON-escaped in the request body.
        "Extract the value of \\\"email\\\" from the text",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(
        &mut session,
        "RETURN ai::extract('reach bob at bob@example.test', 'email');",
    );
    assert_eq!(answer, Value::String("bob@example.test".to_owned()));
}

#[test]
fn a_translate_call_names_the_language_and_answers_the_reply() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"bonjour\"}}]}",
        "Translate the following text into French",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(&mut session, "RETURN ai::translate('hello', 'French');");
    assert_eq!(answer, Value::String("bonjour".to_owned()));
}

#[test]
fn a_classify_reply_naming_a_label_answers_that_label() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"billing\\n\"}}]}",
        "exactly one of these labels: billing, technical",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(
        &mut session,
        "RETURN ai::classify('I was charged twice', ['billing', 'technical']);",
    );
    // The reply carried a newline; the label answers in its declared spelling.
    assert_eq!(answer, Value::String("billing".to_owned()));
}

#[test]
fn a_classify_reply_naming_no_label_answers_none() {
    let store = store();
    let stub = stub(
        "200 OK",
        "{\"choices\":[{\"message\":{\"content\":\"something else\"}}]}",
        "billing",
        1,
    );
    let mut session = declared(&store, &stub, "1000");
    let answer = value(
        &mut session,
        "RETURN ai::classify('I was charged twice', ['billing', 'technical']);",
    );
    // A reply naming none of the declared labels is NONE, never a label the
    // call did not offer.
    assert_eq!(answer, Value::None);
}

// ── C8-C: the plan says what a read will spend ──────────────────────────────

#[test]
fn explain_counts_the_provider_calls_a_projection_makes() {
    let store = store();
    let mut session = declared(&store, &stub("200 OK", "{}", "never reached", 0), "1000");
    session
        .run_now("DEFINE COLLECTION tickets; CREATE tickets:1 = { body: 'a' };")
        .unwrap();
    let plan = value(
        &mut session,
        "EXPLAIN SELECT ai::classify(body, ['a', 'b']) AS label, \
         ai::summarize(body) AS short FROM tickets;",
    );
    let Value::Object(fields) = plan else {
        panic!("EXPLAIN answers with an object");
    };
    assert_eq!(
        fields.get("ai-calls"),
        Some(&Value::Number(tessari_types::Number::Integer(2))),
        "the plan did not count the two provider calls: {fields:?}"
    );
    // A read whose projection holds no provider call carries no such word.
    let plain = value(&mut session, "EXPLAIN SELECT body FROM tickets;");
    let Value::Object(fields) = plain else {
        panic!("EXPLAIN answers with an object");
    };
    assert!(
        !fields.contains_key("ai-calls"),
        "a plain read claimed provider calls: {fields:?}"
    );
}
