//! The one-shot maintenance modes: backup, restore, verify and the health probe.

use std::fs;
use std::io::Write;

use crate::session::Ended;
use tessaridb::Db;

/// Write the store's log to a file.
///
/// The whole store, because state is a pure function of the log — so this is a
/// complete backup and not a partial one, and restoring it is a replay.
pub(crate) fn backup(db: &Db, path: &std::path::Path, from: Option<u64>) -> Result<(), String> {
    let mut out = std::io::BufWriter::new(
        fs::File::create(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    // No `FROM` is the whole store, and the whole store is every log it holds.
    // A `FROM` names one sequence, which counts in one log — so it is the
    // incremental path, and a store holding several logs refuses it rather than
    // writing a file that reads as whole and is missing the rest (Q-624).
    let written = match from {
        None | Some(0 | 1) => tessari_backup::write(db.store(), &mut out),
        Some(from) => {
            let home = tessari_backup::only_log(db.store()).map_err(|why| why.to_string())?;
            tessari_backup::write_from(db.store(), &mut out, home, tessaridb::Sequence::new(from))
        }
    }
    .map_err(|failure| format!("{}: {failure}", path.display()))?;
    out.flush()
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
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

/// Replay a file into an empty store.
pub(crate) fn restore(db: &Db, path: &std::path::Path, upto: Option<u64>) -> Result<(), String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
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
pub(crate) fn verify(path: &std::path::Path) -> Result<Ended, String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
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
