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

use tessari_ql::BackupForm;
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
        name: &str,
    ) -> Result<Outcome> {
        let folder = self.backups.as_deref().ok_or(Error::NoBackupFolder)?;
        let target = inside(folder, name)?;
        if target.symlink_metadata().is_ok() {
            return Err(Error::BackupExists {
                path: target.display().to_string(),
            });
        }
        let bytes = match self.backup(from, form)? {
            Outcome::Value(Value::Bytes(bytes)) => bytes,
            Outcome::Value(Value::String(text)) => text.into_bytes(),
            other => {
                return Err(Error::BackupFailed {
                    reason: format!("the backup answered {other:?} rather than a file"),
                });
            }
        };
        settle(&target, &bytes, form)?;
        let mut answer = std::collections::BTreeMap::new();
        answer.insert(
            "path".to_owned(),
            Value::String(target.display().to_string()),
        );
        let size = i64::try_from(bytes.len()).map_err(|_| Error::BackupFailed {
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

/// The file `name` names inside `folder`, with every folder on the way made.
///
/// Walked one part at a time rather than joined and created in one call,
/// because creating a whole path follows a symlink partway along it — and the
/// folder it would create would already be outside before any check ran.
fn inside(folder: &Path, name: &str) -> Result<PathBuf> {
    let refused = |reason: &str| Error::BackupNameRefused {
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    if name.ends_with('/') || name.ends_with(std::path::MAIN_SEPARATOR) {
        return Err(refused("it names a folder, not a file"));
    }
    let mut parts = Vec::new();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => return Err(refused("`..` would leave the backup folder")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(refused(
                    "it is absolute; name a file inside the backup folder",
                ));
            }
            Component::CurDir => return Err(refused("`.` is not a file name")),
        }
    }
    let Some((file, folders)) = parts.split_last() else {
        return Err(refused("it names no file"));
    };
    let failed = |what: &Path, failure: std::io::Error| Error::BackupFailed {
        reason: format!("{}: {failure}", what.display()),
    };
    fs::create_dir_all(folder).map_err(|failure| failed(folder, failure))?;
    let mut at = folder
        .canonicalize()
        .map_err(|failure| failed(folder, failure))?;
    for part in folders {
        at.push(part);
        match at.symlink_metadata() {
            Ok(held) if held.is_dir() => {}
            Ok(_) => {
                return Err(refused(
                    "a part of it is a link or a file, and is not followed out of the folder",
                ));
            }
            Err(_) => fs::create_dir(&at).map_err(|failure| failed(&at, failure))?,
        }
    }
    at.push(file);
    Ok(at)
}

/// Write `bytes` beside `target`, read them back where they landed, and only
/// then put them at `target`.
fn settle(target: &Path, bytes: &[u8], form: BackupForm) -> Result<()> {
    let mut partial = target.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let written = write_new(&partial, bytes)
        .and_then(|()| check(&partial, form))
        .and_then(|()| fs::rename(&partial, target).map_err(|failure| failure.to_string()));
    if let Err(why) = written {
        drop(fs::remove_file(&partial));
        return Err(Error::BackupFailed {
            reason: format!("{}: {why}", target.display()),
        });
    }
    // The rename is durable only once the folder holding it is.
    if let Some(parent) = target.parent() {
        fs::File::open(parent)
            .and_then(|folder| folder.sync_all())
            .map_err(|failure| Error::BackupFailed {
                reason: format!("{}: {failure}", parent.display()),
            })?;
    }
    Ok(())
}

/// Create `path` — never open one that exists — and sync `bytes` into it.
fn write_new(path: &Path, bytes: &[u8]) -> std::result::Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|failure| failure.to_string())?;
    file.write_all(bytes)
        .map_err(|failure| failure.to_string())?;
    file.sync_all().map_err(|failure| failure.to_string())
}

/// Read the file back through the verifier, where it was written.
///
/// A script has no verifier: it is text the store reads by running it.
fn check(path: &Path, form: BackupForm) -> std::result::Result<(), String> {
    let open = || {
        fs::File::open(path)
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
        BackupForm::Script => Ok(()),
    }
}
