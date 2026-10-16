//! Every synchronous backend answers the asynchronous trait exactly as it answers itself.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::{
    AsyncKvBackend, ErrorCategory, Key, KeyRange, Keyspace, MemoryBackend, ScanRequest, Value,
    WriteBatch,
};

/// The answer of a future that must complete on its first poll.
fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(future).poll(&mut context) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => None,
    }
    .expect("a synchronous backend's future is ready on its first poll")
}

fn key(text: &str) -> Key {
    Key::from_slice(text.as_bytes())
}

fn value(text: &str) -> Value {
    Value::from_slice(text.as_bytes())
}

#[test]
fn a_memory_backend_answers_through_the_async_trait() {
    let backend: Arc<dyn AsyncKvBackend> = Arc::new(MemoryBackend::new());
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, key("a"), value("1"))
        .put(Keyspace::DATA, key("b"), value("2"))
        .put(Keyspace::INDEX, key("a"), value("other keyspace"));
    ready(backend.apply(batch)).expect("applied");
    assert_eq!(
        ready(backend.get(Keyspace::DATA, &key("a"))).expect("read"),
        Some(value("1"))
    );
    assert_eq!(
        ready(backend.get(Keyspace::DATA, &key("z"))).expect("read"),
        None
    );
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::all()).reversed();
    let pairs = ready(backend.scan(&request)).expect("scanned");
    assert_eq!(
        pairs,
        vec![(key("b"), value("2")), (key("a"), value("1"))],
        "reverse order, one keyspace"
    );
    assert_eq!(
        ready(backend.count(Keyspace::DATA, &KeyRange::all())).expect("counted"),
        2
    );
    ready(backend.delete_range(Keyspace::DATA, &KeyRange::between(key("a"), key("b"))))
        .expect("deleted");
    assert!(!ready(backend.contains(Keyspace::DATA, &key("a"))).expect("checked"));
    assert!(ready(backend.contains(Keyspace::DATA, &key("b"))).expect("checked"));
}

#[test]
fn a_failed_precondition_is_a_conflict_through_the_async_trait() {
    let backend: Arc<dyn AsyncKvBackend> = Arc::new(MemoryBackend::new());
    let refused = ready(
        backend.apply(
            WriteBatch::new()
                .expect_value(Keyspace::META, key("k"), value("expected"))
                .put(Keyspace::META, key("k"), value("new")),
        ),
    )
    .expect_err("precondition fails");
    assert_eq!(refused.category(), ErrorCategory::Conflict);
    assert_eq!(
        ready(backend.get(Keyspace::META, &key("k"))).expect("read"),
        None
    );
}
