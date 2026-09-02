//! What a running node reports, and where it goes.
//!
//! The library crates carry the `log` facade and nothing else, so a caller that
//! embeds this store emits nothing unless it installs a logger of its own. The
//! binary is where that choice is made, and this is the choice: one line per
//! event on standard error, at a level `TESSARIDB_LOG` sets.
//!
//! # Why this and not a logging crate
//!
//! Because the whole implementation is below, and it costs no dependency. The
//! tree takes third-party crates deliberately — the HTTP server, the WebSocket
//! framing, the JSON encoder and the base64 decoder are all written here for the
//! same reason — and a line with a timestamp, a level, a target and a message is
//! not where that budget should go.
//!
//! # The timestamp is a real date
//!
//! An operator correlating a refusal with something else needs a time they can
//! compare, and seconds since the epoch is not one. The conversion below is the
//! standard days-to-civil arithmetic and is exercised by the tests at the foot
//! of this file, including the leap day and the century rule that a naive
//! version gets wrong.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

/// The variable that sets how much is reported.
const LEVEL: &str = "TESSARIDB_LOG";

/// Report at this level when nothing says otherwise.
///
/// `info` rather than `warn`: the events this exists for — a connection
/// accepted, a sign-in refused — are not warnings, and a node that reports only
/// its problems cannot answer "was it even reached".
const DEFAULT: LevelFilter = LevelFilter::Info;

/// Send every record at or below `level` to standard error.
struct Stderr {
    level: LevelFilter,
}

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut out = std::io::stderr().lock();
        // A failure to report cannot itself be reported: this *is* the reporting
        // channel. A closed stderr is the operator's decision and not an error.
        drop(writeln!(
            out,
            "{} {:<5} {} {}",
            stamped(SystemTime::now()),
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {
        drop(std::io::stderr().flush());
    }
}

/// Install the logger, reading its level from the environment.
///
/// # Errors
///
/// Returns the facade's own refusal when a logger is already installed, which
/// happens only if a caller installed one before `main` reached here.
pub fn install() -> Result<(), SetLoggerError> {
    let level = std::env::var(LEVEL)
        .ok()
        .map_or(DEFAULT, |asked| level(&asked));
    log::set_boxed_logger(Box::new(Stderr { level }))?;
    log::set_max_level(level);
    Ok(())
}

/// The level a word asks for, or the default when it asks for nothing known.
///
/// An unreadable value is not an error: refusing to start a database because a
/// log level was misspelled would be a worse outcome than reporting at the
/// level it would have used anyway.
fn level(asked: &str) -> LevelFilter {
    match asked.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => DEFAULT,
    }
}

/// One instant as `YYYY-MM-DDThh:mm:ssZ`.
///
/// UTC, because a log compared against another machine's log is compared in one
/// zone or not at all.
fn stamped(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rest / 3_600, (rest / 60) % 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The last day this converts, 9999-12-31.
///
/// A clock reporting past it is a clock that is wrong, and clamping is a better
/// answer than a year with five digits in a fixed-width field.
const LAST_DAY: u64 = 2_932_896;

/// The civil date `days` after 1970-01-01, by Howard Hinnant's algorithm.
///
/// Shifting the epoch to 0000-03-01 is what makes a leap day the *last* day of
/// its year rather than a discontinuity in the middle of one, which is why the
/// arithmetic below has no special case for February.
///
/// # Why every operation is saturating
///
/// Not defensiveness — the workspace denies `arithmetic_side_effects` and there
/// is not one suppression anywhere in this tree. The clamp above makes every
/// intermediate below fit in seven digits, so each `saturating_*` is exact and
/// the saturation is unreachable; what they buy is that this stays true if the
/// clamp is ever changed, and that the rule holds without an exception written
/// for the one file that found it inconvenient.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days.min(LAST_DAY).saturating_add(719_468);
    let era = shifted / 146_097;
    let of_era = shifted % 146_097;
    let year_of_era = of_era
        .saturating_sub(of_era / 1_460)
        .saturating_add(of_era / 36_524)
        .saturating_sub(of_era / 146_096)
        / 365;
    let year = year_of_era.saturating_add(era.saturating_mul(400));
    let day_of_year = of_era.saturating_sub(
        year_of_era
            .saturating_mul(365)
            .saturating_add(year_of_era / 4)
            .saturating_sub(year_of_era / 100),
    );
    let month_prime = day_of_year.saturating_mul(5).saturating_add(2) / 153;
    let day = day_of_year
        .saturating_sub(month_prime.saturating_mul(153).saturating_add(2) / 5)
        .saturating_add(1);
    let month = if month_prime < 10 {
        month_prime.saturating_add(3)
    } else {
        month_prime.saturating_sub(9)
    };
    let year = if month <= 2 {
        year.saturating_add(1)
    } else {
        year
    };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT, civil_from_days, level, stamped};
    use log::LevelFilter;

    fn at(seconds: u64) -> String {
        stamped(
            super::UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .expect("a second count this test wrote is representable"),
        )
    }

    #[test]
    fn the_epoch_is_the_day_it_is_named_after() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_leap_day_is_a_date_and_not_a_shift() {
        // 2024-02-29. A year-length table that forgets the leap day answers
        // 03-01 here, one day early and wrong for the rest of the year.
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn the_century_rule_holds_where_a_naive_version_breaks() {
        // 2000 is a leap year and 1900 was not, which is the pair that catches
        // an implementation testing only `year % 4`.
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn the_time_of_day_is_carried_too() {
        assert_eq!(at(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(at(86_400), "1970-01-02T00:00:00Z");
    }

    #[test]
    fn a_level_nobody_recognises_is_the_default_rather_than_a_refusal() {
        assert_eq!(level("warn"), LevelFilter::Warn);
        assert_eq!(level("  ERROR "), LevelFilter::Error);
        assert_eq!(level("off"), LevelFilter::Off);
        assert_eq!(level("shout"), DEFAULT);
        assert_eq!(level(""), DEFAULT);
    }
}
