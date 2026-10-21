//! A value computed from what several keys hold, written to one of them only
//! while it still holds what was read: `SINTERSTORE`, `ZUNIONSTORE`,
//! `ZRANGESTORE`, `LMPOP`, `ZMPOP` (G079 C6).
//!
//! The keys are read in one statement, so they are one snapshot; the one write
//! is conditioned on the written key's own value (`SET … IF =`, `SET … IF
//! ABSENT`, `DEL … IF =`), and a write that lost a race is computed again from
//! a fresh read. The only key written is the one the condition names, so the
//! command takes effect as if it ran alone at the moment it read.

use std::time::{Duration, Instant};

use tessari_constants::{COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, RESP_CHANGE_PATIENCE_MILLIS};
use tessaridb::Value;

use crate::codec::Reply;
use crate::surface::procedures::Against;
use crate::surface::replies;
use crate::surface::statement::Binder;

/// What a rewrite computes from the values its keys hold.
pub(crate) type Compute = Box<dyn Fn(&[Value]) -> Result<Rewritten, Reply> + Send + Sync>;

/// The write a rewrite makes, and its reply once it has.
pub(crate) struct Rewritten {
    /// Which of the keys read is written.
    pub(crate) key: usize,
    pub(crate) write: Write,
    pub(crate) reply: Reply,
}

/// What the written key is to hold.
pub(crate) enum Write {
    /// Nothing changes; the reply stands.
    Nothing,
    /// This value, keeping the key's expiry or not.
    Hold { value: Value, keep_expiry: bool },
    /// No key at all.
    Remove,
}

impl Against<'_> {
    /// Read `keys`, compute, and write one of them while it still holds what
    /// was read — again from a fresh read when another write came first.
    pub(crate) async fn rewrite(&mut self, keys: &[Vec<u8>], compute: &Compute) -> Reply {
        let mut pause = COMMIT_BACKOFF_STEP;
        let patience = Instant::now()
            .checked_add(Duration::from_millis(RESP_CHANGE_PATIENCE_MILLIS))
            .unwrap_or_else(Instant::now);
        while Instant::now() < patience {
            let mut binder = Binder::default();
            let reads: Vec<String> = keys
                .iter()
                .map(|key| format!("GET {}", binder.key(self.space, key)))
                .collect();
            let held = match self
                .value(format!("RETURN [{}]", reads.join(", ")), binder)
                .await
            {
                Ok(held) => values_of_array(held),
                Err(failed) => return replies::failed(&failed),
            };
            let rewritten = match compute(&held) {
                Ok(rewritten) => rewritten,
                Err(refused) => return refused,
            };
            let (Some(key), Some(old)) = (keys.get(rewritten.key), held.get(rewritten.key)) else {
                return replies::syntax();
            };
            let mut binder = Binder::default();
            let target = binder.key(self.space, key);
            let statement = match rewritten.write {
                Write::Nothing => return rewritten.reply,
                Write::Remove if *old == Value::None => return rewritten.reply,
                Write::Remove => format!("DEL {target} IF = {}", binder.bind(old.clone())),
                Write::Hold { value, keep_expiry } => {
                    let bound = binder.bind(value);
                    let condition = if *old == Value::None {
                        "IF ABSENT".to_owned()
                    } else {
                        format!("IF = {}", binder.bind(old.clone()))
                    };
                    let expiry = if keep_expiry {
                        format!(" EXPIRE (TTL {target} ?? NULL)")
                    } else {
                        String::new()
                    };
                    format!("SET {target} = {bound} {condition}{expiry}")
                }
            };
            match self.value(statement, binder).await {
                Ok(Value::Bool(true)) => return rewritten.reply,
                Ok(_) => {
                    tokio::time::sleep(Duration::from_micros(pause)).await;
                    pause = pause.saturating_mul(2).min(COMMIT_BACKOFF_CEILING);
                }
                Err(failed) => return replies::failed(&failed),
            }
        }
        Reply::error("TRYAGAIN the keys kept changing; try again")
    }
}

fn values_of_array(held: Value) -> Vec<Value> {
    match held {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}
