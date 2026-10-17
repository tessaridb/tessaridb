//! Reads: a key, a range in either direction, the first pair of many ranges, a count.

use std::ops::Bound;

use tessari_kv::{Key, KeyRange, Keyspace, Result, ScanDirection, ScanRequest, Value};
use tessarikv::ReadOptions;

use super::TessariKvBackend;
use super::errors::from_engine;

/// How many pairs one engine page holds: a bounded stride, so an unlimited scan or a count
/// does not ask the engine for the whole range at once.
const PAGE_ENTRIES: usize = 1024;

/// A range in the engine's terms: the first key it may hold, and the first key past it.
pub(super) struct Span {
    pub(super) start: Vec<u8>,
    pub(super) end: Option<Vec<u8>>,
}

impl Span {
    /// `range` as a half-open span; an excluded start and an included end move to the next
    /// key, which is the key followed by a zero byte.
    pub(super) fn of(range: &KeyRange) -> Self {
        let start = match range.start() {
            Bound::Included(key) => key.as_slice().to_vec(),
            Bound::Excluded(key) => successor(key),
            Bound::Unbounded => Vec::new(),
        };
        let end = match range.end() {
            Bound::Included(key) => Some(successor(key)),
            Bound::Excluded(key) => Some(key.as_slice().to_vec()),
            Bound::Unbounded => None,
        };
        Self { start, end }
    }
}

fn successor(key: &Key) -> Vec<u8> {
    let mut bytes = key.as_slice().to_vec();
    bytes.push(0x00);
    bytes
}

fn options(fill_cache: bool) -> ReadOptions {
    let mut options = ReadOptions::default();
    options.fill_cache = fill_cache;
    options
}

impl TessariKvBackend {
    pub(super) async fn read_one(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        let id = self.keyspace(keyspace)?;
        let value = self
            .db
            .get(id, key.as_slice(), &ReadOptions::default())
            .await
            .map_err(|error| from_engine(&error))?;
        Ok(value.map(Value::new))
    }

    pub(super) async fn read_range(
        &self,
        request: &ScanRequest,
        fill_cache: bool,
    ) -> Result<Vec<(Key, Value)>> {
        if request.range.is_provably_empty() {
            return Ok(Vec::new());
        }
        let id = self.keyspace(request.keyspace)?;
        let span = Span::of(&request.range);
        let limit = request.limit.unwrap_or(usize::MAX);
        match request.direction {
            ScanDirection::Forward => self.forward(id, span, limit, fill_cache).await,
            ScanDirection::Reverse => self.reverse(id, span, limit, fill_cache).await,
        }
    }

    /// Up to `limit` pairs of `span`, ascending, read a bounded page at a time.
    async fn forward(
        &self,
        id: tessarikv::KeyspaceId,
        span: Span,
        limit: usize,
        fill_cache: bool,
    ) -> Result<Vec<(Key, Value)>> {
        let options = options(fill_cache);
        let mut collected = Vec::new();
        let mut start = span.start;
        while collected.len() < limit {
            let room = limit.saturating_sub(collected.len()).min(PAGE_ENTRIES);
            let page = self
                .db
                .scan(id, &start, span.end.as_deref(), room, &options)
                .await
                .map_err(|error| from_engine(&error))?;
            collected.extend(
                page.entries
                    .into_iter()
                    .map(|(key, value)| (Key::new(key), Value::new(value))),
            );
            match page.next {
                Some(next) => start = next,
                None => break,
            }
        }
        Ok(collected)
    }

    /// Up to `limit` pairs of `span`, descending from its last key.
    async fn reverse(
        &self,
        id: tessarikv::KeyspaceId,
        span: Span,
        limit: usize,
        fill_cache: bool,
    ) -> Result<Vec<(Key, Value)>> {
        let mut options = options(fill_cache);
        options.lower_bound = Some(span.start);
        options.upper_bound = span.end;
        let mut iterator = self
            .db
            .iter(id, &options)
            .map_err(|error| from_engine(&error))?;
        iterator
            .seek_to_last()
            .await
            .map_err(|error| from_engine(&error))?;
        let mut collected = Vec::new();
        while iterator.valid() && collected.len() < limit {
            collected.push((
                Key::from_slice(iterator.key()),
                Value::from_slice(iterator.value()),
            ));
            iterator.prev().await.map_err(|error| from_engine(&error))?;
        }
        Ok(collected)
    }

    /// The first pair of each range: the same forward read of one pair [`Self::read_range`]
    /// makes, so a batched answer cannot bound differently from a single one (contract 6).
    pub(super) async fn read_firsts(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        let mut found = Vec::with_capacity(ranges.len());
        for range in ranges {
            let request = ScanRequest::new(keyspace, range.clone()).with_limit(1);
            found.push(self.read_range(&request, true).await?.into_iter().next());
        }
        Ok(found)
    }

    /// How many pairs `range` holds, counted a bounded page at a time.
    pub(super) async fn read_count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        if range.is_provably_empty() {
            return Ok(0);
        }
        let id = self.keyspace(keyspace)?;
        let span = Span::of(range);
        let options = options(true);
        let mut total: u64 = 0;
        let mut start = span.start;
        loop {
            let page = self
                .db
                .scan(id, &start, span.end.as_deref(), PAGE_ENTRIES, &options)
                .await
                .map_err(|error| from_engine(&error))?;
            total = total.saturating_add(u64::try_from(page.entries.len()).unwrap_or(u64::MAX));
            match page.next {
                Some(next) => start = next,
                None => return Ok(total),
            }
        }
    }
}
