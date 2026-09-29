//! `ASOF JOIN` — each left record paired with the newest right record at or
//! before its time (ADR-0088 §4, G044 C5). The oracle is a brute-force nested
//! loop over what the test wrote, over a thousand randomized cases.

#![allow(clippy::panic, clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE SERIES quotes RETAIN 36500d TIME at; \
             DEFINE SERIES trades RETAIN 36500d TIME at;",
        )
        .unwrap();
    session
}

/// A small generator, so the cases are reproducible without a dependency.
struct Draw(u64);

impl Draw {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

fn at(millis: u64) -> String {
    let seconds = millis / 1_000;
    format!(
        "datetime '2026-09-29T00:{:02}:{:02}.{:03}Z'",
        seconds / 60,
        seconds % 60,
        millis % 1_000
    )
}

#[test]
fn every_trade_gets_the_newest_quote_at_or_before_it_or_none() {
    let mut draw = Draw(0x5eed);
    for case in 0..1_000 {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut session = opened(&store);
        // Quotes at distinct instants per symbol, so "newest" has one answer.
        let mut quotes: Vec<(u64, u64, u64)> = Vec::new();
        let mut script = String::from("BEGIN;");
        for n in 0..(draw.next(12) + 1) {
            let (sym, when) = (draw.next(3), n * 1_000 + draw.next(900));
            quotes.push((sym, when, n));
            script.push_str(&format!(
                " CREATE quotes = {{ sym: {sym}, px: {n}, at: {} }};",
                at(when)
            ));
        }
        let mut trades: Vec<(u64, u64, u64)> = Vec::new();
        for n in 0..(draw.next(10) + 1) {
            // Sometimes exactly on a quote's instant, to test "at or before".
            let when = if n % 3 == 0 && !quotes.is_empty() {
                quotes[usize::try_from(draw.next(quotes.len() as u64)).unwrap()].1
            } else {
                draw.next(14_000)
            };
            trades.push((draw.next(3), when, n));
            script.push_str(&format!(
                " CREATE trades = {{ sym: {}, id: {n}, at: {} }};",
                trades.last().unwrap().0,
                at(when)
            ));
        }
        script.push_str(" COMMIT;");
        session.run(&script).unwrap();

        let expected: BTreeMap<u64, Option<u64>> = trades
            .iter()
            .map(|(sym, when, n)| {
                let matched = quotes
                    .iter()
                    .filter(|(qs, qw, _)| qs == sym && qw <= when)
                    .max_by_key(|(_, qw, _)| *qw)
                    .map(|(_, _, px)| *px);
                (*n, matched)
            })
            .collect();

        let Outcome::Records { records, .. } = session
            .run("SELECT * FROM trades ASOF JOIN quotes ON trades.sym = quotes.sym;")
            .unwrap()
            .pop()
            .unwrap()
        else {
            panic!("a read answers with records");
        };
        assert_eq!(
            records.len(),
            trades.len(),
            "case {case}: one row per trade"
        );
        let answered: BTreeMap<u64, Option<u64>> = records
            .into_iter()
            .map(|(_, row)| {
                let Value::Object(row) = row else {
                    panic!("a row is an object")
                };
                let Value::Object(trade) = &row["trades"] else {
                    panic!("the trade")
                };
                let id = trade["id"].to_string().parse().unwrap();
                let px = row.get("quotes").map(|quote| {
                    let Value::Object(quote) = quote else {
                        panic!("the quote")
                    };
                    quote["px"].to_string().parse().unwrap()
                });
                (id, px)
            })
            .collect();
        assert_eq!(answered, expected, "case {case}");
    }
}

#[test]
fn an_asof_join_needs_two_event_time_series() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = opened(&store);
    session
        .run("DEFINE SERIES arrivals RETAIN 1d; DEFINE COLLECTION plain;")
        .unwrap();
    for other in ["arrivals", "plain"] {
        let refused = session
            .run(&format!(
                "SELECT * FROM trades ASOF JOIN {other} ON trades.sym = {other}.sym;"
            ))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("declared with `TIME`"), "{refused}");
    }
}
