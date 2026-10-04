use super::*;

/// One `<name>=<value>`, with the value read as a TessariQL literal.
///
/// TessariQL rather than JSON because the console already reads and writes it: what
/// an answer prints pastes back into the next statement, and a parameter written
/// the way an answer is printed closes that loop — `dec 12.34`, `2s` and
/// `datetime '…'` all say themselves.
///
/// The value is parsed **in isolation** by `tessaridb::value_of`, so it is a value
/// or it is nothing: `--param x="1; DROP TABLE users"` is refused as a literal
/// rather than smuggled in as a statement. That reader is shared with the HTTP
/// body's `parameters`, so the two surfaces cannot come to read a supplied value
/// differently.
pub(super) fn parameter(given: &str) -> Result<(String, Value), String> {
    let Some((name, written)) = given.split_once('=') else {
        return Err(format!("--param wants <name>=<value>, not {given:?}"));
    };
    if name.is_empty() {
        return Err("--param wants a name before the `=`".to_owned());
    }
    let name = name.strip_prefix('$').unwrap_or(name);
    let value =
        tessaridb::value_of(written).map_err(|reason| format!("--param {name}: {reason}"))?;
    Ok((name.to_owned(), value))
}

/// Who to say we are, when a name was given.
///
/// The password is read from the environment because an argument would be
/// readable by anybody with the process table and would outlive the session in
/// the shell history.
pub fn credentials(user: Option<String>) -> Result<Option<(String, String)>, String> {
    let Some(name) = user else {
        return Ok(None);
    };
    let password = env::var(PASSWORD)
        .map_err(|_| format!("--user needs the password in {PASSWORD}, and it is not set"))?;
    Ok(Some((name, password)))
}

/// Read an unseal period written as a TessariQL duration (ADR-0092 D4).
///
/// The flag and `TESSARIDB_UNSEAL_FOR` both come through here, so the two
/// spellings cannot accept different things. Zero is refused rather than read
/// as "never": a period is how long the store stays open, and one that means
/// the opposite of what it says at its smallest value is a trap.
///
/// # Errors
///
/// A text that is not a duration, or a duration that is not positive.
pub fn unseal_period(written: &str) -> Result<core::time::Duration, String> {
    let Ok(tessari_types::Value::Duration(period)) = tessaridb::value_of(written) else {
        return Err(format!(
            "wants a duration such as 10m or 1h, not `{written}`"
        ));
    };
    let seconds = u64::try_from(period.seconds()).ok();
    match seconds.map(|seconds| core::time::Duration::new(seconds, period.nanos())) {
        Some(held) if !held.is_zero() => Ok(held),
        _ => Err(format!("wants a period longer than zero, not `{written}`")),
    }
}

/// `TESSARIDB_RETAIN_RECORDS`: how many log records a serving node keeps where
/// no `DEFINE NODE RETAIN` said (ADR-0094 D2) — a positive count, or `none` for
/// an unbounded log.
///
/// Anything else stops the start rather than falling back to the default: an
/// operator who set the variable believes the log is bounded where they said.
///
/// # Errors
///
/// Returns the sentence naming what was wrong with `written`.
pub fn retained_records(written: &str) -> Result<tessari_storage::Retention, String> {
    if written.eq_ignore_ascii_case("none") {
        return Ok(tessari_storage::Retention::Unbounded);
    }
    match written.parse::<u64>() {
        Ok(count) if count > 0 => Ok(tessari_storage::Retention::Keep(
            tessari_types::Sequence::new(count),
        )),
        _ => Err(format!(
            "wants a number of records above zero, or `none`, not `{written}`"
        )),
    }
}
