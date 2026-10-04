// Test assertions are exactly where a panic is the correct outcome.
#![allow(clippy::panic, clippy::unwrap_used)]

use super::{
    ENTITY, FIELD_AWARENESS, FIELD_BALANCE, FIELD_CAMPAIGN, FIELD_COLLECTION, FIELD_EPOCH,
    FIELD_LEASE, FIELD_ROUND, FIELD_VERSION, FailoverDefinition,
};
use std::collections::BTreeMap;
use std::time::Duration as Elapsed;
use tessari_types::{Duration, Epoch, Number, Value};

use crate::error::Error;
use crate::failover::Failover;

fn at(epoch: u64, version: u64) -> FailoverDefinition {
    FailoverDefinition {
        policy: Failover::DEFAULT,
        epoch: Epoch::new(epoch),
        version,
        balance_leaderships: false,
    }
}

#[test]
fn a_policy_survives_the_round_trip_including_its_sub_second_remainder() {
    // A remainder is the part a seconds-only encoding would lose, and it
    // would lose it silently: the row would read back as a policy that
    // still satisfies every relation, just not the one that was written.
    let policy = Failover::stated(
        Elapsed::new(30, 500_000_000),
        Elapsed::from_secs(20),
        Elapsed::from_secs(3),
        Elapsed::from_secs(5),
        Elapsed::from_secs(40),
    )
    .unwrap();
    let written = FailoverDefinition {
        policy,
        epoch: Epoch::new(7),
        version: 2,
        balance_leaderships: false,
    };

    let read = FailoverDefinition::from_value(&written.to_value().unwrap()).unwrap();
    assert_eq!(read, written);
    assert_eq!(read.policy.awareness(), Elapsed::new(30, 500_000_000));
}

#[test]
fn asking_for_balanced_leaderships_survives_and_not_asking_writes_nothing() {
    // A policy stored before the clause existed must read back as it was
    // written, so the field is absent unless it was asked for.
    let unasked = at(1, 0).to_value().unwrap();
    let Value::Object(fields) = &unasked else {
        panic!("a policy is an object");
    };
    assert!(!fields.contains_key(FIELD_BALANCE));
    let asked = FailoverDefinition {
        balance_leaderships: true,
        ..at(1, 0)
    };
    let read = FailoverDefinition::from_value(&asked.to_value().unwrap()).unwrap();
    assert!(read.balance_leaderships);
    assert_eq!(read, asked);
}

#[test]
fn a_higher_pair_supersedes_and_an_equal_or_lower_one_does_not() {
    assert!(at(3, 0).supersedes(&at(2, 9)), "a newer leadership wins");
    assert!(
        at(2, 1).supersedes(&at(2, 0)),
        "a second setting under one leadership wins"
    );
    assert!(!at(2, 0).supersedes(&at(2, 0)), "an equal pair is ignored");
    assert!(
        !at(2, 0).supersedes(&at(2, 1)),
        "a lower version is ignored"
    );
    assert!(
        !at(2, 9).supersedes(&at(3, 0)),
        "a superseded leadership does not win on version — this is the \
             partitioned ex-leader reconnecting, and ordering by arrival or by \
             a clock gets it backwards"
    );
}

#[test]
fn a_stored_row_whose_periods_no_longer_hold_together_is_refused_on_read() {
    // The row is built field by field rather than through `stated`,
    // because `stated` is what this is proving cannot be bypassed: a row
    // written by a build whose relations differed, or edited in place, is
    // refused at the boundary instead of becoming a policy nothing checked.
    let value = Value::Object(BTreeMap::from([
        (
            FIELD_AWARENESS.to_owned(),
            Value::Duration(Duration::from_seconds(10)),
        ),
        (
            FIELD_COLLECTION.to_owned(),
            Value::Duration(Duration::from_seconds(10)),
        ),
        (
            FIELD_ROUND.to_owned(),
            Value::Duration(Duration::from_seconds(1)),
        ),
        // Four times the round, where the window admits two.
        (
            FIELD_CAMPAIGN.to_owned(),
            Value::Duration(Duration::from_seconds(4)),
        ),
        (
            FIELD_LEASE.to_owned(),
            Value::Duration(Duration::from_seconds(10)),
        ),
        (FIELD_EPOCH.to_owned(), Value::Number(Number::Integer(1))),
        (FIELD_VERSION.to_owned(), Value::Number(Number::Integer(1))),
    ]));

    let refused = FailoverDefinition::from_value(&value)
        .expect_err("a row whose campaign outruns its window is not a policy");
    assert!(
        matches!(refused, Error::FailoverCampaignOutpaced { .. }),
        "the relation's own refusal, not a generic malformed-row error: the \
             row is well formed and the policy is not. Got {refused}"
    );
}

#[test]
fn a_missing_field_is_named_rather_than_defaulted() {
    let mut fields = match at(1, 1).to_value().unwrap() {
        Value::Object(fields) => fields,
        other => panic!("a definition encodes as an object, got {other:?}"),
    };
    fields.remove(FIELD_VERSION);

    let refused = FailoverDefinition::from_value(&Value::Object(fields))
        .expect_err("a version that is absent is not a version of zero");
    match refused {
        Error::CatalogMalformed { entity, field, .. } => {
            assert_eq!(entity, ENTITY);
            assert_eq!(field, FIELD_VERSION);
        }
        other => panic!("expected a malformed-row refusal, got {other}"),
    }
}

#[test]
fn a_period_stored_as_a_number_is_refused_rather_than_coerced() {
    // The store has a duration type, so a number here is a row somebody
    // else wrote. Coercing it would make the unit a convention instead of a
    // type, and the unit is the whole question.
    let mut fields = match at(1, 1).to_value().unwrap() {
        Value::Object(fields) => fields,
        other => panic!("a definition encodes as an object, got {other:?}"),
    };
    fields.insert(FIELD_LEASE.to_owned(), Value::Number(Number::Integer(10)));

    let refused = FailoverDefinition::from_value(&Value::Object(fields)).expect_err("ten of what?");
    match refused {
        Error::CatalogMalformed { field, found, .. } => {
            assert_eq!(field, FIELD_LEASE);
            assert_eq!(found, "number");
        }
        other => panic!("expected a malformed-row refusal, got {other}"),
    }
}
