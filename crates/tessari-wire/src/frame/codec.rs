use super::*;

/// A length-prefixed string, the shape every text in a body takes.
pub(crate) fn put_text(into: &mut Vec<u8>, text: &str) {
    let length = u32::try_from(text.len()).unwrap_or(u32::MAX);
    into.extend_from_slice(&length.to_be_bytes());
    into.extend_from_slice(text.as_bytes());
}

/// Read one back, and how much of the buffer it used.
pub(crate) fn take_text(from: &[u8], at: usize) -> Result<(String, usize)> {
    let (bytes, next) = take_bytes(from, at)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Malformed)?;
    Ok((text, next))
}

/// A length-prefixed byte string.
pub(crate) fn put_bytes(into: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    into.extend_from_slice(&length.to_be_bytes());
    into.extend_from_slice(bytes);
}

/// Read one back.
pub(crate) fn take_bytes(from: &[u8], at: usize) -> Result<(Vec<u8>, usize)> {
    let end = at.checked_add(4).ok_or(Error::Malformed)?;
    let header = from.get(at..end).ok_or(Error::Malformed)?;
    let length = usize::try_from(u32::from_be_bytes([
        *header.first().ok_or(Error::Malformed)?,
        *header.get(1).ok_or(Error::Malformed)?,
        *header.get(2).ok_or(Error::Malformed)?,
        *header.get(3).ok_or(Error::Malformed)?,
    ]))
    .unwrap_or(0);
    let stop = end.checked_add(length).ok_or(Error::Malformed)?;
    let bytes = from.get(end..stop).ok_or(Error::Malformed)?;
    Ok((bytes.to_vec(), stop))
}

/// A `u32`, for counts.
pub(crate) fn put_u32(into: &mut Vec<u8>, value: u32) {
    into.extend_from_slice(&value.to_be_bytes());
}

/// A `u64`, for a position in the log.
pub(crate) fn put_u64(into: &mut Vec<u8>, value: u64) {
    into.extend_from_slice(&value.to_be_bytes());
}

/// Read one back.
pub(crate) fn take_u64(from: &[u8], at: usize) -> Result<(u64, usize)> {
    let end = at.checked_add(8).ok_or(Error::Malformed)?;
    let bytes = from.get(at..end).ok_or(Error::Malformed)?;
    let mut held = [0_u8; 8];
    held.copy_from_slice(bytes);
    Ok((u64::from_be_bytes(held), end))
}

/// Read one back.
pub(crate) fn take_u32(from: &[u8], at: usize) -> Result<(u32, usize)> {
    let end = at.checked_add(4).ok_or(Error::Malformed)?;
    let bytes = from.get(at..end).ok_or(Error::Malformed)?;
    Ok((
        u32::from_be_bytes([
            *bytes.first().ok_or(Error::Malformed)?,
            *bytes.get(1).ok_or(Error::Malformed)?,
            *bytes.get(2).ok_or(Error::Malformed)?,
            *bytes.get(3).ok_or(Error::Malformed)?,
        ]),
        end,
    ))
}

/// Append a log home as the nine fixed bytes a reach takes on this wire.
///
/// Written here rather than borrowed from the key encoding, for the reason
/// `tessari-backup`'s own copy records: a wire format is its own format. The key
/// grammar may be re-laid out without every peer in a running cluster having to
/// be upgraded in the same breath, and two formats moving together by accident
/// is exactly what keeping them apart prevents.
///
/// The variant leads, then the namespace and the database, both always written.
/// Fixed width because a frame body that followed it would otherwise start at
/// three different offsets.
///
/// A shard (variant 3) is the one wider home: its table and shard follow the
/// nine bytes. The variant fixes the width, so a body still starts at one
/// offset per variant, and a peer that predates shards refuses the variant
/// rather than reading a table id as the next field.
pub(crate) fn put_reach(into: &mut Vec<u8>, reach: Reach) {
    let (variant, namespace, database) = match reach {
        Reach::Store => (0_u8, 0_u32, 0_u32),
        Reach::Namespace(namespace) => (1, namespace.get(), 0),
        Reach::Database(namespace, database) => (2, namespace.get(), database.get()),
        Reach::Shard(namespace, database, _, _) => (3, namespace.get(), database.get()),
    };
    into.push(variant);
    put_u32(into, namespace);
    put_u32(into, database);
    if let Reach::Shard(_, _, table, shard) = reach {
        put_u32(into, table.get());
        put_u32(into, shard.get());
    }
}

/// Read one back.
///
/// A variant this build does not know is refused rather than widened to the
/// store, which is the key encoding's rule and holds for the same reason: a peer
/// asking for a home this binary cannot name must not be answered with every
/// tenancy's records.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the bytes are short or name a variant this
/// build does not have.
pub(crate) fn take_reach(from: &[u8], at: usize) -> Result<(Reach, usize)> {
    let variant = *from.get(at).ok_or(Error::Malformed)?;
    let (namespace, at) = take_u32(from, at.checked_add(1).ok_or(Error::Malformed)?)?;
    let (database, at) = take_u32(from, at)?;
    let reach = match variant {
        0 => Reach::Store,
        1 => Reach::Namespace(NamespaceId::new(namespace)),
        2 => Reach::Database(NamespaceId::new(namespace), DatabaseId::new(database)),
        3 => {
            let (table, after) = take_u32(from, at)?;
            let (shard, after) = take_u32(from, after)?;
            if shard == 0 {
                return Err(Error::Malformed);
            }
            return Ok((
                Reach::Shard(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                    TableId::new(table),
                    ShardId::new(shard),
                ),
                after,
            ));
        }
        _ => return Err(Error::Malformed),
    };
    Ok((reach, at))
}

/// A log's name on the wire: its home, then the writer allocating into it.
///
/// Fixed width for the reason [`put_reach`] is, and the writer is written
/// always rather than only where a range admits two — a body that followed a
/// sometimes-present field would start at two offsets, and a peer that guessed
/// wrong would read a sequence out of a node identifier.
pub(crate) fn put_log(into: &mut Vec<u8>, log: LogId) {
    put_reach(into, log.home);
    into.extend_from_slice(&log.writer.bytes());
}

/// Read one back.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the bytes are short or the home names a
/// variant this build does not have.
pub(crate) fn take_log(from: &[u8], at: usize) -> Result<(LogId, usize)> {
    let (home, at) = take_reach(from, at)?;
    let end = at.checked_add(NODE_ID_LEN).ok_or(Error::Malformed)?;
    let writer: [u8; NODE_ID_LEN] = from
        .get(at..end)
        .ok_or(Error::Malformed)?
        .try_into()
        .map_err(|_| Error::Malformed)?;
    Ok((LogId::new(home, Writer::new(writer)), end))
}
