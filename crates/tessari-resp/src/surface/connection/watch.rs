//! `WATCH`: the values some keys held when watched, and the guards an `EXEC`
//! runs first so that its transaction is refused when any of them changed.
//!
//! A guard is `THROW … IF (GET key) != <held>` inside the same transaction as
//! the queued commands, so a change made before the `EXEC` refuses it there,
//! and one made while it runs is a commit conflict that runs it — guards and
//! all — again. Changed is judged by value: a key written back to exactly what
//! it held is not seen as changed (Q-G079-10).

use tessaridb::Value;

use super::Connection;
use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::replies;
use crate::surface::statement::Binder;

/// The message a watched key's guard refuses with, which `EXEC` answers as
/// the transaction not having run.
pub(super) const WATCHED: &str = "a watched key changed";

impl Connection {
    /// `WATCH key …`: what each key holds now, kept for the next `EXEC`.
    pub(super) async fn watch(&mut self, keys: Vec<Vec<u8>>) -> Reply {
        let mut against = match self.against() {
            Ok(against) => against,
            Err(reply) => return reply,
        };
        let mut binder = Binder::default();
        let reads: Vec<String> = keys
            .iter()
            .map(|key| format!("GET {}", binder.key(against.space, key)))
            .collect();
        let held = match against
            .value(format!("RETURN [{}]", reads.join(", ")), binder)
            .await
        {
            Ok(Value::Array(held)) => held,
            Ok(_) => return replies::syntax(),
            Err(failed) => return replies::failed(&failed),
        };
        self.watched.extend(keys.into_iter().zip(held));
        Reply::OK
    }
}

/// One guard per watched key, bound into the transaction's own parameters.
pub(super) fn guards(
    watched: Vec<(Vec<u8>, Value)>,
    space: &Space,
    binder: &mut Binder,
) -> Vec<String> {
    watched
        .into_iter()
        .map(|(key, held)| {
            let target = binder.key(space, &key);
            let held = binder.bind(held);
            format!("THROW '{WATCHED}' IF (GET {target}) != {held}")
        })
        .collect()
}
