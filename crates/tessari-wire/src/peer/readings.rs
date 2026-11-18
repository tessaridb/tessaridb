//! The readings a node serves when a new enough peer asks: what this process
//! measured of its own machine, and how old the measurement already is.
//!
//! # Why a follow-up frame and not a field on the greeting
//!
//! The greeting's body is scanned to its last byte by every build that reads
//! it (`Hello::decode` reaches for the placed line at the tail), so a new
//! field there would be misread — as a line, a malformed greeting, a closed
//! connection — by any released build. The ask instead rides the follow-up
//! the door already reads after the greeting, and is only ever sent to a peer
//! whose last greeting named a build new enough to answer it. Peers of an
//! earlier build never see the frame, which is what lets the wire shape move
//! without a rolling upgrade breaking the cluster.
//!
//! # What the body is
//!
//! The sample's age in whole seconds first — what the peer said — then a
//! presence byte and six fixed slots, zero when the figure is absent. Every
//! `u64` is a legitimate value, so presence is a bitmask and never a sentinel:
//! a figure the node could not measure is *absent*, not *zero*, and the two
//! must not look alike on the wire either.

use core::time::Duration;

use tessari_storage::Readings;

use super::hello::whole_seconds;
use crate::error::{Error, Result};
use crate::frame;

/// The first build whose peers answer a readings ask.
pub const READINGS_FROM: tessari_encoding::NodeVersion = tessari_encoding::NodeVersion {
    major: 0,
    minor: 49,
    patch: 0,
};

/// What a node serves of its own resources: the sample, and how old it
/// already was at the moment of serving, stated on the server's own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedReadings {
    /// The sample as the server's sampler last published it.
    pub sample: Readings,
    /// How old the sample was when the answer was written.
    pub age: Duration,
}

const SLOT_COUNT: usize = 8;

/// The presence bit of each field, in the order the slots travel.
const BITS: [u8; SLOT_COUNT] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80];

/// The body of a readings answer frame.
#[must_use]
pub fn encode_served(served: &ServedReadings) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + 1 + 8 * SLOT_COUNT);
    frame::put_u64(&mut body, whole_seconds(served.age));
    let mask = mask_of(&served.sample);
    body.push(mask);
    for value in slots(&served.sample) {
        frame::put_u64(&mut body, value.unwrap_or(0));
    }
    body
}

/// Read one back.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the body is not exactly the shape a
/// readings answer takes. The frame only travels between builds new enough to
/// know it, so a short or long body is a truncation or a bug and is refused
/// rather than guessed at.
pub fn decode_served(body: &[u8]) -> Result<ServedReadings> {
    if body.len() != 8 + 1 + 8 * SLOT_COUNT {
        return Err(Error::Malformed);
    }
    let (age, at) = frame::take_u64(body, 0)?;
    let mask = *body.get(at).ok_or(Error::Malformed)?;
    let mut at = at.checked_add(1).ok_or(Error::Malformed)?;
    let mut slots = [None; SLOT_COUNT];
    for (slot, bit) in slots.iter_mut().zip(BITS) {
        let (value, next) = frame::take_u64(body, at)?;
        if mask & bit != 0 {
            *slot = Some(value);
        }
        at = next;
    }
    Ok(ServedReadings {
        sample: from_slots(slots),
        age: Duration::from_secs(age),
    })
}

/// The presence byte for `sample`: a bit per figure the sample holds.
fn mask_of(sample: &Readings) -> u8 {
    slots(sample)
        .iter()
        .zip(BITS)
        .filter(|(value, _)| value.is_some())
        .fold(0, |mask, (_, bit)| mask | bit)
}

/// The six figures in wire order, each absent or as stored.
fn slots(sample: &Readings) -> [Option<u64>; SLOT_COUNT] {
    [
        sample.cpu_cores,
        sample.cpu_busy,
        sample.memory_total,
        sample.memory_used,
        sample.disk_total,
        sample.disk_free,
        sample.network_received,
        sample.network_transmitted,
    ]
}

/// The sample a slot array decodes to, by the same wire order.
fn from_slots(slots: [Option<u64>; SLOT_COUNT]) -> Readings {
    let [
        cpu_cores,
        cpu_busy,
        memory_total,
        memory_used,
        disk_total,
        disk_free,
        network_received,
        network_transmitted,
    ] = slots;
    Readings {
        cpu_cores,
        cpu_busy,
        memory_total,
        memory_used,
        disk_total,
        disk_free,
        network_received,
        network_transmitted,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn full() -> Readings {
        Readings {
            cpu_cores: Some(8),
            cpu_busy: Some(4_200),
            memory_total: Some(16_000_000_000),
            memory_used: Some(4_000_000_000),
            disk_total: Some(500_000_000_000),
            disk_free: Some(300_000_000_000),
            network_received: Some(1_000_000),
            network_transmitted: Some(500_000),
        }
    }

    #[test]
    fn a_served_sample_round_trips_with_its_age() {
        let served = ServedReadings {
            sample: full(),
            age: Duration::from_secs(2),
        };
        let read = decode_served(&encode_served(&served)).expect("a served sample reads back");
        assert_eq!(read, served);
    }

    #[test]
    fn a_fractional_age_rounds_up_so_a_sample_is_never_younger_than_it_was() {
        let served = ServedReadings {
            sample: full(),
            age: Duration::from_millis(1_500),
        };
        let read = decode_served(&encode_served(&served)).expect("a served sample reads back");
        assert_eq!(read.age, Duration::from_secs(2), "up, never down");
    }

    #[test]
    fn absent_fields_stay_absent_and_never_read_as_zero() {
        let served = ServedReadings {
            sample: Readings {
                cpu_cores: Some(4),
                cpu_busy: None,
                memory_total: None,
                memory_used: Some(1_000),
                disk_total: None,
                disk_free: None,
                network_received: None,
                network_transmitted: Some(10),
            },
            age: Duration::ZERO,
        };
        let read = decode_served(&encode_served(&served)).expect("a partial sample reads back");
        assert_eq!(read, served);
        assert_eq!(read.sample.cpu_busy, None, "absent is not zero");
    }

    #[test]
    fn a_body_of_any_other_length_is_refused_not_guessed() {
        let mut body = encode_served(&ServedReadings {
            sample: full(),
            age: Duration::ZERO,
        });
        assert!(decode_served(&body[..body.len() - 1]).is_err(), "short");
        body.push(0);
        assert!(decode_served(&body).is_err(), "long");
        assert!(decode_served(&[]).is_err(), "empty");
    }
}
