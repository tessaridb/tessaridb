//! How often a caller nobody signed in may append to a topic, on this node.
//!
//! # Why it is counted in memory and not in the store
//!
//! Because the bound protects this node, and a count kept in the log would
//! charge every anonymous append a second write to say it happened — the cost
//! the bound exists to limit. So each process keeps its own count and never
//! persists it: a restart forgets it and lets at most one more burst through,
//! and a cluster of `n` nodes admits `n` times the rate. Both are stated where
//! the rate is documented, because a limit somebody believes is store-wide is a
//! limit they will size wrongly.
//!
//! # Why a map keyed by table is bounded
//!
//! The keys are topics an owner declared `PUBLIC`, never anything a caller
//! supplies, so the map grows with the catalog and not with the traffic. That
//! is the difference from sign-in throttling, whose key is a name an attacker
//! chooses and which therefore uses a fixed table instead.
//!
//! # The rule
//!
//! Each topic keeps one instant: the moment its allowance will next be fully
//! earned. An append of `n` messages costs `n` intervals of `per / rate`, and
//! is admitted while that leaves the instant no more than `per` ahead of now.
//! So a quiet topic accepts `rate` messages at once, and then one every
//! `per / rate`: the long-run average is `rate` per `per`, and no window of
//! length `per` holds more than twice that. A statement carrying more messages
//! than `rate` can never be admitted, and is refused rather than trimmed.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use tessari_types::TableId;

use crate::catalog::PublicAppend;

/// When each public topic's allowance is next fully earned, on this node.
#[derive(Debug, Default)]
pub(crate) struct PublicRates {
    /// One entry per public topic, each updated under its own shard's lock
    /// — two topics being appended to never wait on each other.
    earned: DashMap<TableId, Instant>,
}

impl PublicRates {
    /// Whether `count` more anonymous messages may be appended to `topic` at
    /// `now`, taking them from its allowance when they may.
    pub(crate) fn admit(
        &self,
        topic: TableId,
        rule: PublicAppend,
        count: u64,
        now: Instant,
    ) -> bool {
        let Some(window) = window(rule) else {
            return false;
        };
        let Some(cost) = cost(window, rule.rate, count) else {
            return false;
        };
        // The entry is held for the read, the check and the write, so two
        // appenders to one topic cannot both spend the same allowance.
        let mut earned = self.earned.entry(topic).or_insert(now);
        let from = (*earned).max(now);
        let Some(next) = from.checked_add(cost) else {
            return false;
        };
        if next.saturating_duration_since(now) > window {
            return false;
        }
        *earned = next;
        true
    }
}

/// The declared window as a clock length; `None` for one no clock can hold.
fn window(rule: PublicAppend) -> Option<Duration> {
    let seconds = u64::try_from(rule.per.seconds()).ok()?;
    Some(Duration::new(seconds, rule.per.nanos()))
}

/// What `count` messages cost against an allowance of `rate` per `window`.
fn cost(window: Duration, rate: u64, count: u64) -> Option<Duration> {
    let rate = u32::try_from(rate).unwrap_or(u32::MAX);
    let interval = window.checked_div(rate)?;
    interval.checked_mul(u32::try_from(count).ok()?)
}

#[cfg(test)]
mod tests;
