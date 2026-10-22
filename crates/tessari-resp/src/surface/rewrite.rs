//! A value computed from what several keys hold, written to one of them only
//! while it still holds what was read: `SINTERSTORE`, `ZUNIONSTORE`,
//! `ZRANGESTORE`, `LMPOP`, `ZMPOP` (G079 C6).
//!
//! The keys are read in one statement, so they are one snapshot; the one write
//! is conditioned on the written key's own value (`SET … IF =`, `SET … IF
//! ABSENT`, `DEL … IF =`), and a write that lost a race is computed again from
//! a fresh read. The only key written is the one the condition names, so the
//! command takes effect as if it ran alone at the moment it read.

use std::time::Instant;

use tessaridb::{Value, back_off};

use crate::codec::Reply;
use crate::surface::procedures::{Against, patience};
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
    /// This value, keeping the key's expiry or not.
    Hold { value: Value, keep_expiry: bool },
    /// No key at all.
    Remove,
}

impl Against<'_> {
    /// Read `keys`, compute, and write one of them while it still holds what
    /// was read — again from a fresh read when another write came first.
    pub(crate) async fn rewrite(&mut self, keys: &[Vec<u8>], compute: &Compute) -> Reply {
        let patience = patience();
        let mut attempt = 0_u32;
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
                    attempt = attempt.saturating_add(1);
                    back_off(attempt).await;
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
