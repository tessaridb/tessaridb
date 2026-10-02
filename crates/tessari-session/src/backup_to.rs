//! `BACKUP … TO '<name>'`: the same backup, written into the node's backup
//! folder instead of answered with.
//!
//! The node writes a file only inside the folder its operator gave it, so the
//! name is checked before anything is created: a relative path of plain parts,
//! each folder on the way made one at a time and never a symlink, and a file
//! that is already there never replaced. The bytes go to `<name>.partial`, are
//! synced and read back through the verifier where they were written, and only
//! then renamed into place — a backup directory never holds a file that reads
//! as a backup and is not one (ADR-0091 §8).

use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use tessari_ql::{BackupForm, ReachRef};
use tessari_types::{Number, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Take the backup `BACKUP` would answer with and write it to `name`
    /// inside the backup folder, answering where it landed and its size.
    pub(crate) fn backup_to(
        &self,
        from: Option<u64>,
        form: BackupForm,
        of: &[ReachRef],
        name: &str,
    ) -> Result<Outcome> {
        let folder = self.backups.as_deref().ok_or(Error::NoBackupFolder)?;
        let target = inside(folder, name)?;
        if target.symlink_metadata().is_ok() {
            return Err(Error::BackupExists {
                path: target.display().to_string(),
            });
        }
        // A snapshot goes from the read straight into the file, chunk by chunk,
        // so the node's memory does not grow with the store (ADR-0094 D6). The
        // other forms are answered whole and written as they were answered.
        let size = if form == BackupForm::State {
            let within = self.state_scope(of)?;
            self.refuse_a_partial_snapshot(within)?;
            settle(&target, form, self.at_rest.as_deref(), |file| {
                let out = std::io::BufWriter::new(file);
                match &self.at_rest {
                    Some(key) => {
                        let mut sealing =
                            key.seal_into(out).map_err(|failure| failure.to_string())?;
                        tessari_backup::write_state_within(self.store, within, &mut sealing)
                            .map_err(|failure| failure.to_string())?;
                        sealing
                            .finish()
                            .and_then(|mut out| out.flush())
                            .map_err(|failure| failure.to_string())
                    }
                    None => {
                        let mut out = out;
                        tessari_backup::write_state_within(self.store, within, &mut out)
                            .map_err(|failure| failure.to_string())?;
                        out.flush().map_err(|failure| failure.to_string())
                    }
                }
            })?
        } else {
            let bytes = match self.backup(from, form, of)? {
                Outcome::Value(Value::Bytes(bytes)) => bytes,
                Outcome::Value(Value::String(text)) => text.into_bytes(),
                other => {
                    return Err(Error::BackupFailed {
                        reason: format!("the backup answered {other:?} rather than a file"),
                    });
                }
            };
            settle(&target, form, self.at_rest.as_deref(), |file| {
                file.write_all(&bytes)
                    .map_err(|failure| failure.to_string())
            })?
        };
        let mut answer = std::collections::BTreeMap::new();
        answer.insert(
            "path".to_owned(),
            Value::String(target.display().to_string()),
        );
        let size = i64::try_from(size).map_err(|_| Error::BackupFailed {
            reason: "the backup is larger than a count can say".to_owned(),
        })?;
        answer.insert("bytes".to_owned(), Value::Number(Number::Integer(size)));
        let form = match form {
            BackupForm::Log => "log",
            BackupForm::State => "state",
            BackupForm::Script => "script",
        };
        answer.insert("form".to_owned(), Value::String(form.to_owned()));
        Ok(Outcome::Value(Value::Object(answer)))
    }
}

impl Session<'_> {
    /// Refuse a snapshot of `within` on a node that holds only part of it
    /// (ADR-0094 D7) — a backup assembled from what one node happens to hold
    /// is not one state of anything. A node never served anything holds all it
    /// has, and pays one in-memory read here.
    ///
    /// The refusal is the one a read gets, naming a table this node lacks and,
    /// where the table is split, the shards it lacks — which are named first,
    /// since they say where the rest of the table is.
    pub(crate) fn refuse_a_partial_snapshot(&self, within: tessari_types::Reach) -> Result<()> {
        if self.store.served().is_none() {
            return Ok(());
        }
        let mut transaction = self.store.begin()?;
        let mut tables = Vec::new();
        {
            let catalog = tessari_storage::Catalog::new(&mut transaction);
            for namespace in catalog.namespaces()? {
                for database in catalog.databases_in(namespace.id)? {
                    let place = tessari_types::Reach::Database(namespace.id, database.id);
                    if within.contains(place) {
                        tables.extend(catalog.tables_in(namespace.id, database.id)?);
                    }
                }
            }
        }
        let mut lacking = Vec::new();
        for table in tables {
            if let Some(missing) =
                self.missing(&mut transaction, table.id, crate::evaluate::Part::Whole)?
            {
                lacking.push(missing.refusal(None));
            }
        }
        lacking.sort_by_key(
            |refusal| !matches!(refusal, Error::NotHeldHere { shards, .. } if !shards.is_empty()),
        );
        lacking.into_iter().next().map_or(Ok(()), Err)
    }
}

/// Where a snapshot goes when the caller streams it, taken once.
pub(crate) struct Sink(std::cell::RefCell<Option<Box<dyn std::io::Write + Send>>>);

impl Sink {
    pub(crate) const fn none() -> Self {
        Self(std::cell::RefCell::new(None))
    }

    pub(crate) fn to(out: Box<dyn std::io::Write + Send>) -> Self {
        Self(std::cell::RefCell::new(Some(out)))
    }

    /// The sink, the first time it is asked for.
    pub(crate) fn take(&self) -> Option<Box<dyn std::io::Write + Send>> {
        self.0.borrow_mut().take()
    }
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.0.try_borrow().map_or(true, |held| held.is_some());
        formatter.debug_tuple("Sink").field(&held).finish()
    }
}

/// Why a name is refused, as the refusal carries it.
fn name_refused(name: &str, reason: &str) -> Error {
    Error::BackupNameRefused {
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

/// A name's parts, when every one is plain: the folders on the way, and the file.
fn parts_of(name: &str) -> Result<(Vec<&std::ffi::OsStr>, &std::ffi::OsStr)> {
    if name.ends_with('/') || name.ends_with(std::path::MAIN_SEPARATOR) {
        return Err(name_refused(name, "it names a folder, not a file"));
    }
    let mut parts = Vec::new();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                return Err(name_refused(name, "`..` would leave the backup folder"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(name_refused(
                    name,
                    "it is absolute; name a file inside the backup folder",
                ));
            }
            Component::CurDir => return Err(name_refused(name, "`.` is not a file name")),
        }
    }
    let Some(file) = parts.pop() else {
        return Err(name_refused(name, "it names no file"));
    };
    Ok((parts, file))
}

fn failed(what: &Path, failure: &std::io::Error) -> Error {
    Error::BackupFailed {
        reason: format!("{}: {failure}", what.display()),
    }
}

/// The file `name` names inside `folder`, with every folder on the way made.
///
/// Walked one part at a time rather than joined and created in one call,
/// because creating a whole path follows a symlink partway along it — and the
/// folder it would create would already be outside before any check ran.
fn inside(folder: &Path, name: &str) -> Result<PathBuf> {
    let (folders, file) = parts_of(name)?;
    fs::create_dir_all(folder).map_err(|failure| failed(folder, &failure))?;
    let mut at = folder
        .canonicalize()
        .map_err(|failure| failed(folder, &failure))?;
    for part in folders {
        at.push(part);
        match at.symlink_metadata() {
            Ok(held) if held.is_dir() => {}
            Ok(_) => {
                return Err(name_refused(
                    name,
                    "a part of it is a link or a file, and is not followed out of the folder",
                ));
            }
            Err(_) => fs::create_dir(&at).map_err(|failure| failed(&at, &failure))?,
        }
    }
    at.push(file);
    Ok(at)
}

/// The file `name` names inside `folder`, which must already be there as a file
/// and be reached through folders only — nothing is made, and no link is
/// followed, for a read any more than for a write.
pub(crate) fn readable(folder: &Path, name: &str) -> Result<PathBuf> {
    let (folders, file) = parts_of(name)?;
    let mut at = folder
        .canonicalize()
        .map_err(|failure| failed(folder, &failure))?;
    for part in folders {
        at.push(part);
        if !at.symlink_metadata().is_ok_and(|held| held.is_dir()) {
            return Err(name_refused(
                name,
                "no such folder inside the backup folder",
            ));
        }
    }
    at.push(file);
    match at.symlink_metadata() {
        Ok(held) if held.is_file() => Ok(at),
        Ok(_) => Err(name_refused(
            name,
            "it is not a file, and a link is not followed",
        )),
        Err(_) => Err(name_refused(name, "no such file in the backup folder")),
    }
}

/// Write `bytes` beside `target`, read them back where they landed, and only
/// then put them at `target`.
fn settle(
    target: &Path,
    form: BackupForm,
    key: Option<&tessari_vault::AtRestKey>,
    write: impl FnOnce(&mut fs::File) -> std::result::Result<(), String>,
) -> Result<u64> {
    let mut partial = target.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let written = write_new(&partial, write).and_then(|size| {
        check(&partial, form, key)?;
        fs::rename(&partial, target).map_err(|failure| failure.to_string())?;
        Ok(size)
    });
    let size = match written {
        Ok(size) => size,
        Err(why) => {
            drop(fs::remove_file(&partial));
            return Err(Error::BackupFailed {
                reason: format!("{}: {why}", target.display()),
            });
        }
    };
    // The rename is durable only once the folder holding it is.
    if let Some(parent) = target.parent() {
        fs::File::open(parent)
            .and_then(|folder| folder.sync_all())
            .map_err(|failure| Error::BackupFailed {
                reason: format!("{}: {failure}", parent.display()),
            })?;
    }
    Ok(size)
}

/// Create `path` — never open one that exists — write into it, sync it, and
/// answer its size.
fn write_new(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> std::result::Result<(), String>,
) -> std::result::Result<u64, String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|failure| failure.to_string())?;
    write(&mut file)?;
    file.sync_all().map_err(|failure| failure.to_string())?;
    file.metadata()
        .map(|held| held.len())
        .map_err(|failure| failure.to_string())
}

/// Read the file back through the verifier, where it was written — opening it
/// first when this node seals its backups, so a sealed file is checked whole.
///
/// A script has no verifier: it is text the store reads by running it. A
/// sealed one is still read to its end, which is what authenticates it.
fn check(
    path: &Path,
    form: BackupForm,
    key: Option<&tessari_vault::AtRestKey>,
) -> std::result::Result<(), String> {
    let open = || {
        fs::File::open(path)
            .map(std::io::BufReader::new)
            .and_then(|file| tessari_vault::at_rest::reading(key, file))
            .map(std::io::BufReader::new)
            .map_err(|failure| failure.to_string())
    };
    match form {
        BackupForm::Log => {
            let held =
                tessari_backup::verify(&mut open()?).map_err(|failure| failure.to_string())?;
            if held.truncated {
                return Err("it reads back cut short".to_owned());
            }
            Ok(())
        }
        BackupForm::State => tessari_backup::verify_state(&mut open()?)
            .map(|_| ())
            .map_err(|failure| failure.to_string()),
        BackupForm::Script => std::io::copy(&mut open()?, &mut std::io::sink())
            .map(|_| ())
            .map_err(|failure| failure.to_string()),
    }
}
