use super::*;

/// The format this store was written in, if it has been written at all.
///
/// A free function rather than a method because it runs before the store
/// exists: `open` settles the format before it resolves the node identity, and
/// the identity is one of the store's own fields.
pub(super) fn read_format_version(backend: Arc<dyn KvBackend>) -> Result<Option<FormatVersion>> {
    let key = FormatVersionKey.encode();
    let stored = backend.get(FormatVersionKey::keyspace(), &key)?;
    match stored {
        Some(value) => Ok(Some(FormatVersion::decode(value.as_slice())?)),
        None => Ok(None),
    }
}

/// Write the metadata a fresh store needs, refusing if someone raced us.
///
/// The `Absent` precondition is what makes two processes opening the same
/// new store safe: exactly one of them writes the metadata.
/// Refuse a store that holds data and no format version.
///
/// Every store this engine creates is stamped before anything else is written
/// to it, so data without a stamp is data whose format nobody knows; stamping
/// it as new would write this build's format over it. One key per keyspace is
/// read, which is what an empty store costs to tell apart from one that is not.
pub(super) fn refuse_data_without_a_stamp(backend: &dyn KvBackend) -> Result<()> {
    for &keyspace in Keyspace::ALL {
        let first = backend.scan(&ScanRequest::new(keyspace, KeyRange::all()).with_limit(1))?;
        if !first.is_empty() {
            return Err(tessari_encoding::Error::UnstampedStore {
                keyspace: keyspace.name(),
            }
            .into());
        }
    }
    Ok(())
}

pub(super) fn write_initial_metadata(backend: Arc<dyn KvBackend>) -> Result<()> {
    let format_key = FormatVersionKey.encode();
    let batch = WriteBatch::new()
        .expect_absent(FormatVersionKey::keyspace(), format_key.clone())
        .put(
            FormatVersionKey::keyspace(),
            format_key,
            FormatVersion::CURRENT.encode(),
        );
    backend.apply(batch)?;
    Ok(())
}

/// Rewrite a log written before it had homes, once, at open.
///
/// Every record in such a store was written by one leader into one flat log, so
/// [`Reach::Store`] is not a fallback for them — it is the home they actually
/// belong to, and the chain a narrower subscriber reads passes through it. The
/// keys are rewritten rather than read through a second decoder because the old
/// shape and the new one are told apart by length, and a choice made by length
/// on the replication read path is a choice made on every record forever.
///
/// One batch, so a store is either rewritten or untouched. The format version
/// moves inside it, which is what makes a failure retry on the next open
/// instead of leaving half a log in each shape.
///
/// # Errors
///
/// Returns the substrate's failure, and a decoding failure when a log key of
/// the expected old shape does not hold a sequence.
pub(super) fn give_an_older_log_its_home(
    backend: Arc<dyn KvBackend>,
    found: FormatVersion,
) -> Result<()> {
    if found >= FormatVersion::HOMED_LOG {
        return Ok(());
    }
    let prefix = LogKey::prefix();
    let request = ScanRequest {
        keyspace: LogKey::keyspace(),
        range: KeyRange::prefix(&prefix),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let mut batch = WriteBatch::new();
    for (key, value) in backend.scan(&request)? {
        let Some(sequence) = sequence_in_a_homeless_log_key(key.as_slice()) else {
            continue;
        };
        batch = batch.delete(LogKey::keyspace(), key).put(
            LogKey::keyspace(),
            LogKey::new(LogId::unattributed(Reach::Store), sequence).encode(),
            value,
        );
    }
    // The applied position moved the same way, from one singleton to one key
    // per home. Read before the rewrite for the same reason the records are:
    // its old key no longer names anything this build addresses.
    let homeless_applied = Key::from(vec![KeyKind::AppliedPosition.tag()]);
    if let Some(value) = backend.get(AppliedPositionKey::keyspace(), &homeless_applied)? {
        batch = batch
            .delete(AppliedPositionKey::keyspace(), homeless_applied)
            .put(
                AppliedPositionKey::keyspace(),
                AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode(),
                value,
            );
    }
    batch = batch.put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::HOMED_LOG.encode(),
    );
    backend.apply(batch)?;
    Ok(())
}

/// Rewrite a log written before its entries named their writer, once, at open.
///
/// # Why this rewrites rather than reads two shapes
///
/// The writer could have been written only where a range has two of them,
/// leaving a short key and a long one under one tag and telling them apart by
/// length on the read path. [`give_an_older_log_its_home`] already met that
/// choice for the home and refused it, in the sentence directly above: a choice
/// made by length there is a choice made on **every record forever**, instead of
/// once. The same refusal applies to the writer, and it buys more here — a
/// fixed-width writer is what keeps [`LogKey::prefix_for`] exact, and an exact
/// per-log prefix is what stops a scan of one writer's log quietly returning
/// another's.
///
/// # What writer an existing entry gets
///
/// [`Writer::UNATTRIBUTED`], and not this node's own identifier. A store being
/// migrated may be a follower, and a follower's log holds the records the
/// *leader* wrote — attributing them to the machine that happens to be opening
/// the file would be a lie the store then carries as fact. Nobody is the only
/// true answer available.
///
/// One batch, so a store is either rewritten or untouched, with the format
/// version moving inside it — which is what makes a failure retry on the next
/// open rather than leave half a log in each shape.
///
/// # Errors
///
/// Returns the substrate's failure, and a decoding failure when a log key of
/// the expected old shape does not hold a sequence.
pub(super) fn give_an_older_log_its_writer(
    backend: Arc<dyn KvBackend>,
    found: FormatVersion,
) -> Result<()> {
    if found >= FormatVersion::WRITER_QUALIFIED_LOG {
        return Ok(());
    }
    let mut batch = WriteBatch::new();
    let entries = ScanRequest {
        keyspace: LogKey::keyspace(),
        range: KeyRange::prefix(&LogKey::prefix()),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&entries)? {
        let Some((home, sequence)) = an_unqualified_log_key(key.as_slice())? else {
            continue;
        };
        batch = batch.delete(LogKey::keyspace(), key).put(
            LogKey::keyspace(),
            LogKey::new(LogId::unattributed(home), sequence).encode(),
            value,
        );
    }
    // The applied position moved the same way, from one key per home to one per
    // log. Rewritten in the same batch as the entries it accounts for, because a
    // store holding one of the two shapes is a store whose next commit builds on
    // a tail nothing wrote.
    let positions = ScanRequest {
        keyspace: AppliedPositionKey::keyspace(),
        range: KeyRange::prefix(&[KeyKind::AppliedPosition.tag()]),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&positions)? {
        let Some(home) = an_unqualified_position_key(key.as_slice())? else {
            continue;
        };
        batch = batch.delete(AppliedPositionKey::keyspace(), key).put(
            AppliedPositionKey::keyspace(),
            AppliedPositionKey::new(LogId::unattributed(home)).encode(),
            value,
        );
    }
    batch = batch.put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::CURRENT.encode(),
    );
    backend.apply(batch)?;
    Ok(())
}

/// The home and sequence in a log key written before log keys carried a writer,
/// or `None` when the key is already qualified.
///
/// Told apart by length, which is exact **here** and nowhere else: both shapes
/// are fixed-width and differ by the sixteen bytes of the writer. That this is
/// a one-time read at open is the whole difference from making the same test on
/// the replication read path.
///
/// # Errors
///
/// Returns a decoding failure when a key of the old shape does not hold a
/// readable home.
pub(super) fn an_unqualified_log_key(key: &[u8]) -> Result<Option<(Reach, Sequence)>> {
    const UNQUALIFIED_LEN: usize = 1 + REACH_LEN + 8;
    if key.len() != UNQUALIFIED_LEN {
        return Ok(None);
    }
    let home = reach_in(key)?;
    let Some(tail) = key.get(1usize.saturating_add(REACH_LEN)..) else {
        return Ok(None);
    };
    let Ok(bytes) = <[u8; 8]>::try_from(tail) else {
        return Ok(None);
    };
    Ok(Some((home, Sequence::new(u64::from_be_bytes(bytes)))))
}

/// The home in an applied-position key written before positions named a writer,
/// or `None` when the key is already qualified.
///
/// # Errors
///
/// Returns a decoding failure when a key of the old shape does not hold a
/// readable home.
pub(super) fn an_unqualified_position_key(key: &[u8]) -> Result<Option<Reach>> {
    const UNQUALIFIED_LEN: usize = 1 + REACH_LEN;
    if key.len() != UNQUALIFIED_LEN {
        return Ok(None);
    }
    Ok(Some(reach_in(key)?))
}

/// The home an old-shape key leads with, read through the key type that owns
/// the encoding rather than by slicing bytes here.
///
/// The reach sits at the same offset in both keyspaces, so the bytes are
/// re-tagged as an applied position and decoded by the type that owns the
/// layout. Reading nine bytes here by hand would be a second decoder for one
/// encoding, which is the thing this layer exists to prevent.
pub(super) fn reach_in(key: &[u8]) -> Result<Reach> {
    let mut qualified =
        Vec::with_capacity(1usize.saturating_add(REACH_LEN).saturating_add(NODE_ID_LEN));
    qualified.push(KeyKind::AppliedPosition.tag());
    qualified.extend_from_slice(
        key.get(1..1usize.saturating_add(REACH_LEN))
            .unwrap_or_default(),
    );
    qualified.extend_from_slice(&Writer::UNATTRIBUTED.bytes());
    Ok(AppliedPositionKey::decode(&qualified)?.log.home)
}

/// The sequence in a log key written before log keys carried a home, or `None`
/// when the key is already homed.
///
/// Told apart by length, which is exact: both shapes are fixed-width, and they
/// differ by the nine bytes of the home.
pub(super) fn sequence_in_a_homeless_log_key(key: &[u8]) -> Option<Sequence> {
    const HOMELESS_LEN: usize = 9;
    if key.len() != HOMELESS_LEN {
        return None;
    }
    let bytes: [u8; 8] = key.get(1..HOMELESS_LEN)?.try_into().ok()?;
    Some(Sequence::new(u64::from_be_bytes(bytes)))
}

/// Give the version counter a value, once, on a store that has none.
///
/// Every store written before the version was separated from the log position
/// stamped its records at the position, so the position **is** the version
/// those records were written at. Seeding from it is what makes the counter
/// resume rather than restart — a counter that began again at zero would hand
/// out version numbers the store's existing records already hold, and a reader
/// would resolve to whichever of the two the key order happened to put first.
///
/// A fresh store reaches this with the position at zero, so the two cases are
/// one path rather than two that could disagree.
///
/// The `Absent` precondition makes a race between two openers harmless: one
/// writes, the other is refused and finds the value already there. A refusal is
/// therefore success, not an error to report.
pub(super) fn seed_version_position(backend: Arc<dyn KvBackend>) -> Result<()> {
    let version_key = VersionPositionKey.encode();
    if backend
        .get(VersionPositionKey::keyspace(), &version_key)?
        .is_some()
    {
        return Ok(());
    }
    // The store home, because a store written before the log was partitioned
    // had exactly one log and this is where the migration files it.
    let applied_key = AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode();
    let applied = match backend.get(AppliedPositionKey::keyspace(), &applied_key)? {
        Some(value) => Sequence::decode(value.as_slice())?,
        None => Sequence::ZERO,
    };
    let batch = WriteBatch::new()
        .expect_absent(VersionPositionKey::keyspace(), version_key.clone())
        .put(
            VersionPositionKey::keyspace(),
            version_key,
            applied.encode(),
        );
    match backend.apply(batch) {
        Ok(()) | Err(tessari_kv::Error::Conflict { .. }) => Ok(()),
        Err(other) => Err(other.into()),
    }
}
