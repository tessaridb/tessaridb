use std::time::{Duration, Instant};

use tessari_types::TableId;

use super::PublicRates;
use crate::catalog::PublicAppend;

fn rule(rate: u64, seconds: i64) -> PublicAppend {
    PublicAppend {
        rate,
        per: tessari_types::Duration::from_seconds(seconds),
    }
}

#[test]
fn a_quiet_topic_takes_its_rate_at_once_and_then_one_per_interval() {
    let rates = PublicRates::default();
    let topic = TableId::new(40);
    let start = Instant::now();
    let every = rule(4, 60);
    for n in 0..4 {
        assert!(
            rates.admit(topic, every, 1, start),
            "message {n} of the burst"
        );
    }
    assert!(
        !rates.admit(topic, every, 1, start),
        "the fifth in one instant"
    );
    // One interval is a quarter of the window.
    let later = start + Duration::from_secs(14);
    assert!(!rates.admit(topic, every, 1, later), "before the interval");
    let later = start + Duration::from_secs(15);
    assert!(rates.admit(topic, every, 1, later), "at the interval");
    assert!(!rates.admit(topic, every, 1, later), "and only one");
}

#[test]
fn a_statement_is_charged_per_message_and_one_larger_than_the_rate_never_passes() {
    let rates = PublicRates::default();
    let topic = TableId::new(41);
    let start = Instant::now();
    let every = rule(3, 1);
    assert!(
        !rates.admit(topic, every, 4, start),
        "four messages at a rate of three"
    );
    assert!(
        rates.admit(topic, every, 3, start),
        "a refusal took nothing"
    );
    assert!(!rates.admit(topic, every, 1, start));
}

#[test]
fn each_topic_has_its_own_allowance() {
    let rates = PublicRates::default();
    let start = Instant::now();
    let every = rule(1, 60);
    assert!(rates.admit(TableId::new(42), every, 1, start));
    assert!(!rates.admit(TableId::new(42), every, 1, start));
    assert!(rates.admit(TableId::new(43), every, 1, start));
}
