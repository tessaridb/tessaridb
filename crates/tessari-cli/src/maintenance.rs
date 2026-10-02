//! The one-shot maintenance modes: backup, restore, verify and the health probe.

use std::fs;
use std::io::Read;

use crate::session::Ended;
use tessaridb::Db;

/// Write the store's log to a file.
///
/// The whole store, because state is a pure function of the log — so this is a
/// complete backup and not a partial one, and restoring it is a replay.
pub(crate) fn backup(db: &Db, path: &std::path::Path, from: Option<u64>) -> Result<(), String> {
    let written = aside(path, db.at_rest(), |mut out| {
        // No `FROM` is the whole store, and the whole store is every log it holds.
        // A `FROM` names one sequence, which counts in one log — so it is the
        // incremental path, and a store holding several logs refuses it rather than
        // writing a file that reads as whole and is missing the rest (Q-624).
        match from {
            None | Some(0 | 1) => tessari_backup::write(db.store(), &mut out),
            Some(from) => {
                let home = tessari_backup::only_log(db.store()).map_err(|why| why.to_string())?;
                tessari_backup::write_from(
                    db.store(),
                    &mut out,
                    home,
                    tessaridb::Sequence::new(from),
                )
            }
        }
        .map_err(|failure| match failure {
            // The log cannot be backed up whole once it is pruned, and the
            // refusal is only useful if it names what can.
            tessari_backup::Error::Store(tessari_storage::Error::BelowLogStart { .. }) => {
                format!("{failure}; a pruned store is backed up whole with --snapshot")
            }
            other => other.to_string(),
        })
    })?;
    println!("{} record(s) to {}", written.records, path.display());
    for log in &written.logs {
        println!(
            "  {} sequences {}..={}",
            log_name(log.log.home),
            log.from,
            log.tail
        );
    }
    Ok(())
}

/// Write the store's current state to a file (ADR-0091).
///
/// Sized by what the store holds rather than by its history, and the one whole
/// backup a pruned store can still take.
pub(crate) fn snapshot(db: &Db, path: &std::path::Path) -> Result<(), String> {
    let taken = aside(path, db.at_rest(), |mut out| {
        tessari_backup::write_state(db.store(), &mut out).map_err(|failure| failure.to_string())
    })?;
    println!(
        "{} record(s) at version {} to {}",
        taken.records,
        taken.version,
        path.display()
    );
    for (log, at) in &taken.positions {
        println!("  {} at {at}", log_name(log.home));
    }
    Ok(())
}

/// Write the store's current state as TessariQL that rebuilds it (ADR-0091).
///
/// The parts it does not carry are named in the file's own header, and here.
pub(crate) fn dump(db: &Db, path: &std::path::Path) -> Result<(), String> {
    let taken = aside(path, db.at_rest(), |out| {
        let taken =
            tessari_session::write_script(db.store()).map_err(|failure| failure.to_string())?;
        std::io::Write::write_all(out, taken.text.as_bytes())
            .map_err(|failure| failure.to_string())?;
        Ok(taken)
    })?;
    println!(
        "{} record(s) as statements to {}",
        taken.records,
        path.display()
    );
    for part in &taken.refused {
        println!("  not carried: {part}");
    }
    Ok(())
}

/// Write a file beside `path` and move it into place only once it is whole.
///
/// A backup that fails part-way must leave nothing a restore could mistake for
/// one: `--restore` reads a cut file as the prefix it holds and says so on the
/// error stream, and the exit code is still success. Writing in place also
/// destroyed whatever good backup stood at the destination before the attempt.
/// So the bytes go to `<path>.partial`, are flushed and synced, and a rename puts
/// them where they were asked for; any failure removes the partial file instead
/// (ADR-0091 §8).
fn aside<T>(
    path: &std::path::Path,
    key: Option<&tessaridb::AtRestKey>,
    write: impl FnOnce(&mut dyn std::io::Write) -> Result<T, String>,
) -> Result<T, String> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    let written = fs::File::create(&partial)
        .map_err(|failure| failure.to_string())
        .and_then(|file| {
            let mut out = std::io::BufWriter::new(file);
            // Sealed under the store's key when it has one (ADR-0108 D7): a
            // backup of an encrypted store is no less private than the store.
            let written = match key {
                Some(key) => {
                    let mut sealing = key.seal_into(out).map_err(|failure| failure.to_string())?;
                    let written = write(&mut sealing)?;
                    out = sealing.finish().map_err(|failure| failure.to_string())?;
                    written
                }
                None => write(&mut out)?,
            };
            let file = out
                .into_inner()
                .map_err(|failure| failure.error().to_string())?;
            file.sync_all().map_err(|failure| failure.to_string())?;
            Ok(written)
        })
        .and_then(|written| {
            fs::rename(&partial, path).map_err(|failure| failure.to_string())?;
            Ok(written)
        });
    match written {
        Ok(written) => {
            // The rename is durable only once the directory holding it is.
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|failure| format!("{}: {failure}", parent.display()))?;
            }
            Ok(written)
        }
        Err(why) => {
            drop(fs::remove_file(&partial));
            Err(format!("{}: {why}", path.display()))
        }
    }
}

/// Whether the file at `path` is a state snapshot rather than a log.
/// A backup file as it reads: opened with `key` when it was sealed, and as it
/// is when it was not; sealed with no key is refused, saying so.
fn opened(
    path: &std::path::Path,
    key: Option<&tessaridb::AtRestKey>,
) -> std::io::Result<std::io::BufReader<tessari_vault::at_rest::Reading<fs::File>>> {
    fs::File::open(path)
        .and_then(|file| tessari_vault::at_rest::reading(key, file))
        .map(std::io::BufReader::new)
}

fn holds_a_state(
    path: &std::path::Path,
    key: Option<&tessaridb::AtRestKey>,
) -> Result<bool, String> {
    let mut opening = Vec::with_capacity(tessari_backup::STATE_MAGIC.len());
    std::io::Read::take(
        opened(path, key).map_err(|failure| format!("{}: {failure}", path.display()))?,
        u64::try_from(tessari_backup::STATE_MAGIC.len()).unwrap_or(u64::MAX),
    )
    .read_to_end(&mut opening)
    .map_err(|failure| format!("{}: {failure}", path.display()))?;
    Ok(tessari_backup::is_state(&opening))
}

/// Replay a file into an empty store, or write a snapshot's state into one.
pub(crate) fn restore(
    db: &Db,
    path: &std::path::Path,
    upto: Option<u64>,
    sealed_under: Option<&tessaridb::AtRestKey>,
) -> Result<(), String> {
    // A sealed backup opens under the key it was sealed with: the one named,
    // else the store's own — the store being restored into may use another.
    let key = sealed_under.or(db.at_rest());
    if holds_a_state(path, key)? {
        if upto.is_some() {
            return Err(format!(
                "{}: a snapshot is one moment and is restored whole; --upto stops a log replay",
                path.display()
            ));
        }
        let taken = tessari_backup::read_state(db.store(), || opened(path, key))
            .map_err(|failure| format!("{}: {failure}", path.display()))?;
        println!(
            "{} record(s) at version {} from {}",
            taken.records,
            taken.version,
            path.display()
        );
        for (log, at) in &taken.positions {
            println!("  {} at {at}", log_name(log.home));
        }
        return Ok(());
    }
    let mut input =
        opened(path, key).map_err(|failure| format!("{}: {failure}", path.display()))?;
    let upto = upto.map(tessaridb::Sequence::new);
    let held = tessari_backup::read_until(db.store(), &mut input, upto)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!("{} record(s) from {}", held.records, path.display());
    for log in &held.logs {
        println!("  {} through {}", log_name(log.log.home), log.tail);
    }
    if held.truncated {
        // Said loudly and on the error stream, because a partial restore that
        // reads as a success is how somebody learns later that the last hour is
        // gone.
        eprintln!(
            "warning: {} was cut short — {} record(s) across {} log(s) were read",
            path.display(),
            held.records,
            held.logs.len()
        );
    }
    Ok(())
}

/// Name a log the way an operator reads it.
///
/// Numbers rather than names because a backup file holds ids and nothing else:
/// resolving them would need the catalog the file is a copy of, and a restore is
/// exactly the moment that catalog may not be there yet.
pub(crate) fn log_name(home: tessaridb::Reach) -> String {
    match home {
        tessaridb::Reach::Store => "store".to_owned(),
        tessaridb::Reach::Namespace(namespace) => format!("namespace {}", namespace.get()),
        tessaridb::Reach::Database(namespace, database) => {
            format!("namespace {} database {}", namespace.get(), database.get())
        }
        tessaridb::Reach::Shard(namespace, database, table, shard) => format!(
            "namespace {} database {} table {} shard {}",
            namespace.get(),
            database.get(),
            table.get(),
            shard.get()
        ),
    }
}

/// Read a backup and say what it holds, applying none of it.
pub(crate) fn verify(
    path: &std::path::Path,
    key: Option<&tessaridb::AtRestKey>,
) -> Result<Ended, String> {
    if holds_a_state(path, key)? {
        let mut input =
            opened(path, key).map_err(|failure| format!("{}: {failure}", path.display()))?;
        let taken = tessari_backup::verify_state(&mut input)
            .map_err(|failure| format!("{}: {failure}", path.display()))?;
        println!(
            "a snapshot of {} record(s) and {} topic position(s) at version {}, whole",
            taken.records, taken.topics, taken.version
        );
        for (log, at) in &taken.positions {
            println!("  {} at {at}", log_name(log.home));
        }
        return Ok(Ended::Fine);
    }
    let mut input =
        opened(path, key).map_err(|failure| format!("{}: {failure}", path.display()))?;
    let held = tessari_backup::verify(&mut input)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!(
        "{} record(s) across {} log(s)",
        held.records,
        held.logs.len()
    );
    for log in &held.logs {
        println!(
            "  {} sequences {}..={}, good through {}",
            log_name(log.span.log.home),
            log.span.from,
            log.span.tail,
            log.good_through
        );
    }
    if held.truncated {
        // On the error stream and with a non-zero exit, because the whole point
        // of verifying is that somebody's script can act on the answer.
        eprintln!("warning: {} was cut short", path.display());
        for log in &held.logs {
            if log.good_through.get() < log.span.tail.get() {
                eprintln!(
                    "  {} says it holds through {} and reads through {}",
                    log_name(log.span.log.home),
                    log.span.tail,
                    log.good_through
                );
            }
        }
        return Ok(Ended::Refused);
    }
    Ok(Ended::Fine)
}

/// Say whether the store is well.
///
/// The same question `GET /health` answers, for an operator holding a store and
/// no server — which is exactly the situation somebody is in when they are
/// wondering whether it is still keeping their data. Exits non-zero when it is
/// not, so a cron line needs no parsing.
pub(crate) fn health(db: &Db) -> Result<Ended, String> {
    let held = db.store().health().map_err(|failure| failure.to_string())?;
    match held.complaint() {
        None => {
            // "this node" rather than the bare number, and the second clause
            // when there is one. A store whose history was written before logs
            // named their writer holds every record it ever had in a log
            // attributed to nobody, so this node's own position is zero — the
            // same sentence a store nobody has ever written answers, read at
            // the one moment an upgrade makes somebody run this by hand (Q-764).
            match held.elsewhere {
                None => println!(
                    "well — this node has committed to sequence {}",
                    held.committed
                ),
                Some(elsewhere) => println!(
                    "well — this node has committed to sequence {}, and another log in this store reaches sequence {elsewhere}",
                    held.committed
                ),
            }
            Ok(Ended::Fine)
        }
        Some(said) => {
            println!("unwell — {said}");
            Ok(Ended::Refused)
        }
    }
}
