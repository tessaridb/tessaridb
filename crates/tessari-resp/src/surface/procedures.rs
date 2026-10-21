//! The commands that take more than one round trip to the store: walks over
//! keys, changes made between a read and a compare-and-set write, and moves
//! whose refusal is the answer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_constants::{RESP_CHANGE_PATIENCE_MILLIS, RESP_SCAN_CURSORS, RESP_SCAN_PAGE};
use tessari_types::RefusalClass;
use tessaridb::{Outcome, Parameters, RecordId, Value, back_off};

use crate::codec::Reply;
use crate::surface::commands::{Change, Expiry, When, changed, expire, string_reply};
use crate::surface::glob;
use crate::surface::replies;
use crate::surface::running::{Failed, Held};
use crate::surface::statement::{Binder, Planned, value_of, values_of};
use crate::surface::values;
use crate::surface::{Shared, Space};

/// The instant a write that keeps losing races gives up.
pub(crate) fn patience() -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(RESP_CHANGE_PATIENCE_MILLIS))
        .unwrap_or_else(Instant::now)
}

/// What a procedure runs against.
pub(crate) struct Against<'a> {
    pub(crate) shared: &'a Arc<Shared>,
    pub(crate) held: &'a mut Held,
    pub(crate) space: &'a Space,
}

/// Where each `SCAN` cursor a connection handed out stands.
#[derive(Debug, Default)]
pub(crate) struct Cursors {
    next: u64,
    places: VecDeque<(u64, RecordId)>,
}

impl Cursors {
    fn hand_out(&mut self, place: RecordId) -> u64 {
        self.next = self.next.checked_add(1).unwrap_or(1);
        if self.places.len() >= RESP_SCAN_CURSORS {
            self.places.pop_front();
        }
        self.places.push_back((self.next, place));
        self.next
    }

    fn place(&self, cursor: u64) -> Option<RecordId> {
        self.places
            .iter()
            .find(|(handed, _)| *handed == cursor)
            .map(|(_, place)| place.clone())
    }
}

impl Against<'_> {
    /// Run `statements` as one script — one transaction when there is more
    /// than one — retrying while the store says to, for as long as the patience
    /// lasts rather than a count of tries, each wait jittered so that writers
    /// that lost together do not re-race together; the outcomes are the
    /// statements' own.
    pub(crate) async fn run(
        &mut self,
        statements: &[String],
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>, Failed> {
        let together = statements.len() > 1;
        let mut script = String::new();
        if together {
            script.push_str("BEGIN; ");
        }
        for statement in statements {
            script.push_str(statement);
            script.push_str("; ");
        }
        if together {
            script.push_str("COMMIT;");
        }
        let patience = patience();
        let mut attempt = 0_u32;
        loop {
            match self
                .held
                .run(self.shared, self.space, &script, parameters.clone())
                .await
            {
                Ok(mut outcomes) => {
                    if together {
                        outcomes.pop();
                        if !outcomes.is_empty() {
                            outcomes.remove(0);
                        }
                    }
                    return Ok(outcomes);
                }
                Err(Failed::Refused(refused))
                    if refused.class() == RefusalClass::Retry && Instant::now() < patience =>
                {
                    attempt = attempt.saturating_add(1);
                    back_off(attempt).await;
                }
                Err(failed) => return Err(failed),
            }
        }
    }

    /// Run a planned command and decode its reply.
    pub(crate) async fn planned(&mut self, planned: Planned, binder: Binder) -> Reply {
        let parameters = binder.into_parameters();
        match self.run(&planned.statements, &parameters).await {
            Ok(outcomes) => (planned.decode)(outcomes),
            Err(Failed::Refused(refused)) if replies::container(&refused).is_some() => {
                replies::container(&refused).unwrap_or_else(replies::syntax)
            }
            Err(Failed::Refused(refused))
                if refused.class() == RefusalClass::Invalid && planned.on_invalid.is_some() =>
            {
                planned.on_invalid.unwrap_or_else(replies::syntax)
            }
            Err(failed) => replies::failed(&failed),
        }
    }

    /// One statement with its parameters, answering its one value.
    pub(crate) async fn value(
        &mut self,
        statement: String,
        binder: Binder,
    ) -> Result<Value, Failed> {
        let outcomes = self.run(&[statement], &binder.into_parameters()).await?;
        Ok(outcomes.into_iter().next().map_or(Value::None, value_of))
    }

    /// One page of keys after `after`, with `prefix` when it is text.
    async fn page(
        &mut self,
        prefix: Option<&str>,
        after: Option<Value>,
        limit: u64,
    ) -> Result<Vec<RecordId>, Failed> {
        let mut binder = Binder::default();
        let mut statement = format!("KEYS FROM {}", self.space.name);
        if let Some(prefix) = prefix {
            let bound = binder.bind(Value::String(prefix.to_owned()));
            statement.push_str(&format!(" PREFIX {bound}"));
        }
        if let Some(after) = after {
            let bound = binder.bind(after);
            statement.push_str(&format!(" AFTER {bound}"));
        }
        // A whole number written out: `LIMIT` takes no parameter, and this one
        // is the node's own count, never a client's text.
        statement.push_str(&format!(" LIMIT {limit}"));
        let outcomes = self.run(&[statement], &binder.into_parameters()).await?;
        Ok(match outcomes.into_iter().next() {
            Some(Outcome::Keys(keys)) => keys,
            _ => Vec::new(),
        })
    }

    /// Every key from `after` on (with `prefix` when it is text), filtered.
    async fn walk(
        &mut self,
        prefix: Option<&str>,
        mut after: Option<Value>,
        pattern: &[u8],
        found: &mut Vec<Reply>,
    ) -> Result<(), Failed> {
        loop {
            let keys = self.page(prefix, after.take(), RESP_SCAN_PAGE).await?;
            let full = u64::try_from(keys.len()).unwrap_or(u64::MAX) >= RESP_SCAN_PAGE;
            for key in &keys {
                let bytes = values::identity(key);
                if glob::matches(pattern, &bytes) {
                    found.push(Reply::Bulk(bytes));
                }
            }
            match keys.last() {
                Some(last) if full => after = Some(values::resume_at(last)),
                _ => return Ok(()),
            }
        }
    }

    /// `KEYS pattern`: every key matching, the literal prefix sought rather
    /// than read past. Text keys sort before bytes keys and a prefix walk
    /// reads text only, so a prefix pattern reads the bytes keys apart.
    pub(crate) async fn keys(&mut self, pattern: &[u8]) -> Reply {
        let mut found = Vec::new();
        let prefix = std::str::from_utf8(glob::literal_prefix(pattern))
            .ok()
            .filter(|prefix| !prefix.is_empty());
        let walked = match prefix {
            None => self.walk(None, None, pattern, &mut found).await,
            Some(prefix) => match self.walk(Some(prefix), None, pattern, &mut found).await {
                Ok(()) => {
                    self.walk(None, Some(Value::Bytes(Vec::new())), pattern, &mut found)
                        .await
                }
                failed => failed,
            },
        };
        match walked {
            Ok(()) => Reply::Array(found),
            Err(failed) => replies::failed(&failed),
        }
    }

    /// `SCAN cursor [MATCH p] [COUNT n]`: one page, and the cursor to ask for
    /// the next (`0` when the walk is over).
    pub(crate) async fn scan(
        &mut self,
        cursors: &mut Cursors,
        cursor: u64,
        (pattern, kind): (Option<&[u8]>, Option<&str>),
        count: u64,
    ) -> Reply {
        let after = if cursor == 0 {
            None
        } else {
            match cursors.place(cursor) {
                Some(place) => Some(values::resume_at(&place)),
                None => return Reply::error("ERR invalid cursor"),
            }
        };
        let limit = count.min(RESP_SCAN_PAGE);
        let keys = match self.page(None, after, limit).await {
            Ok(keys) => keys,
            Err(failed) => return replies::failed(&failed),
        };
        let full = u64::try_from(keys.len()).unwrap_or(u64::MAX) >= limit;
        let next = match keys.last() {
            Some(last) if full => cursors.hand_out(last.clone()),
            _ => 0,
        };
        let mut matched: Vec<&RecordId> = keys
            .iter()
            .filter(|key| {
                pattern.is_none_or(|pattern| glob::matches(pattern, &values::identity(key)))
            })
            .collect();
        if let Some(kind) = kind {
            match self.kinds(&matched).await {
                Ok(kinds) => {
                    let mut held = kinds.into_iter();
                    matched.retain(|_| held.next() == Some(kind));
                }
                Err(failed) => return replies::failed(&failed),
            }
        }
        let matched = matched
            .into_iter()
            .map(|key| Reply::Bulk(values::identity(key)))
            .collect();
        Reply::Array(vec![
            Reply::Bulk(next.to_string().into_bytes()),
            Reply::Array(matched),
        ])
    }

    /// `RANDOMKEY`: the first key, or nil for an empty space.
    pub(crate) async fn first_key(&mut self) -> Reply {
        match self.page(None, None, 1).await {
            Ok(keys) => keys
                .first()
                .map_or(Reply::Nil, |key| Reply::Bulk(values::identity(key))),
            Err(failed) => replies::failed(&failed),
        }
    }

    /// `APPEND`, `SETRANGE`, `INCRBYFLOAT`: read, change, and write only if
    /// the key still holds what was read — keeping the expiry it has then.
    pub(crate) async fn change(&mut self, key: &[u8], change: &Change) -> Reply {
        let patience = patience();
        let mut attempt = 0_u32;
        while Instant::now() < patience {
            let mut binder = Binder::default();
            let target = binder.key(self.space, key);
            let held = match self.value(format!("GET {target}"), binder).await {
                Ok(held) => held,
                Err(failed) => return replies::failed(&failed),
            };
            let bytes = match string_reply(&held) {
                Reply::Bulk(bytes) => Some(bytes),
                Reply::Nil => None,
                error => return error,
            };
            if let Change::SetRange { bytes: written, .. } = change
                && written.is_empty()
            {
                let length = bytes.as_ref().map_or(0, Vec::len);
                return Reply::Integer(i64::try_from(length).unwrap_or(i64::MAX));
            }
            let next = match changed(bytes.as_deref(), change) {
                Ok(next) => next,
                Err(refused) => return refused,
            };
            let mut binder = Binder::default();
            let target = binder.key(self.space, key);
            let bound = binder.bind(values::stored(&next));
            let statement = if held == Value::None {
                format!("SET {target} = {bound} IF ABSENT")
            } else {
                let old = binder.bind(held);
                format!("SET {target} = {bound} IF = {old} EXPIRE (TTL {target} ?? NULL)")
            };
            match self.value(statement, binder).await {
                Ok(Value::Bool(true)) => {
                    return match change {
                        Change::IncrementFloat(_) => Reply::Bulk(next),
                        _ => Reply::Integer(i64::try_from(next.len()).unwrap_or(i64::MAX)),
                    };
                }
                Ok(_) => {
                    attempt = attempt.saturating_add(1);
                    back_off(attempt).await;
                }
                Err(failed) => return replies::failed(&failed),
            }
        }
        Reply::error("TRYAGAIN the key kept changing; try again")
    }

    /// Whether each of `keys` is there, after a refusal that may have been
    /// the answer.
    async fn present(&mut self, keys: &[&[u8]]) -> Result<Vec<bool>, Failed> {
        let mut binder = Binder::default();
        let reads: Vec<String> = keys
            .iter()
            .map(|key| format!("TTL {}", binder.key(self.space, key)))
            .collect();
        let statement = format!("RETURN [{}]", reads.join(", "));
        let outcomes = self.run(&[statement], &binder.into_parameters()).await?;
        Ok(outcomes
            .into_iter()
            .next()
            .map(values_of)
            .unwrap_or_default()
            .iter()
            .map(|left| *left != Value::None)
            .collect())
    }

    /// `MSETNX`: every key created in one transaction, which a key already
    /// there refuses whole.
    pub(crate) async fn set_if_none_exist(&mut self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Reply {
        // A key named twice takes its last value, as one write.
        let mut last: Vec<(&[u8], &[u8])> = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            match last.iter_mut().find(|(seen, _)| *seen == key.as_slice()) {
                Some(seen) => seen.1 = value,
                None => last.push((key, value)),
            }
        }
        let mut binder = Binder::default();
        let statements: Vec<String> = last
            .iter()
            .map(|(key, value)| {
                let target = binder.key(self.space, key);
                let bound = binder.bind(values::stored(value));
                format!("CREATE {target} = {bound}")
            })
            .collect();
        match self.run(&statements, &binder.into_parameters()).await {
            Ok(_) => Reply::Integer(1),
            Err(failed) => {
                let keys: Vec<&[u8]> = last.iter().map(|(key, _)| *key).collect();
                match self.present(&keys).await {
                    Ok(present) if present.contains(&true) => Reply::Integer(0),
                    _ => replies::failed(&failed),
                }
            }
        }
    }

    /// `RENAME` and `RENAMENX`: the value and its expiry moved in one
    /// transaction; a missing source refuses it, and so — for `RENAMENX` — a
    /// destination already there.
    pub(crate) async fn rename(&mut self, from: &[u8], to: &[u8], only_new: bool) -> Reply {
        if from == to {
            return match self.present(&[from]).await {
                Ok(present) if present.first() == Some(&true) => {
                    if only_new {
                        Reply::Integer(0)
                    } else {
                        Reply::OK
                    }
                }
                Ok(_) => Reply::error("ERR no such key"),
                Err(failed) => replies::failed(&failed),
            };
        }
        let mut binder = Binder::default();
        let source = binder.key(self.space, from);
        let destination = binder.key(self.space, to);
        let mut statements = Vec::with_capacity(3);
        if only_new {
            statements.push(format!("CREATE {destination} = GET {source}"));
        }
        statements.push(format!(
            "SET {destination} = GET {source} EXPIRE TTL {source}"
        ));
        statements.push(format!("DELETE {source}"));
        match self.run(&statements, &binder.into_parameters()).await {
            Ok(_) if only_new => Reply::Integer(1),
            Ok(_) => Reply::OK,
            Err(failed) => match self.present(&[from, to]).await {
                Ok(present) if present.first() == Some(&false) => Reply::error("ERR no such key"),
                Ok(present) if only_new && present.get(1) == Some(&true) => Reply::Integer(0),
                _ => replies::failed(&failed),
            },
        }
    }

    /// `COPY`: the value and its expiry written to a second key; a missing
    /// source answers 0, and so does a destination already there without
    /// `REPLACE`.
    pub(crate) async fn copy(&mut self, from: &[u8], to: &[u8], replace: bool) -> Reply {
        let mut binder = Binder::default();
        let source = binder.key(self.space, from);
        let destination = binder.key(self.space, to);
        let mut statements = Vec::with_capacity(2);
        if !replace {
            statements.push(format!("CREATE {destination} = GET {source}"));
        }
        statements.push(format!(
            "SET {destination} = GET {source} EXPIRE TTL {source}"
        ));
        match self.run(&statements, &binder.into_parameters()).await {
            Ok(_) => Reply::Integer(1),
            Err(failed) => match self.present(&[from, to]).await {
                Ok(present)
                    if present.first() == Some(&false)
                        || (!replace && present.get(1) == Some(&true)) =>
                {
                    Reply::Integer(0)
                }
                _ => replies::failed(&failed),
            },
        }
    }

    /// `EXPIRE … NX|XX|GT|LT`: the expiry the key has, then the new one when
    /// the condition holds. A key with no expiry counts as expiring never.
    pub(crate) async fn expire_when(&mut self, key: &[u8], at: Expiry, when: When) -> Reply {
        let mut binder = Binder::default();
        let target = binder.key(self.space, key);
        let left = match self.value(format!("RETURN TTL {target}"), binder).await {
            Ok(left) => left,
            Err(failed) => return replies::failed(&failed),
        };
        let current = match left {
            Value::None => return Reply::Integer(0),
            Value::Null => None,
            Value::Duration(span) => Some(
                span.seconds()
                    .saturating_mul(1_000)
                    .saturating_add(i64::from(span.nanos() / 1_000_000)),
            ),
            _ => return Reply::Integer(0),
        };
        let now = crate::surface::commands::now_millis();
        let new = match at {
            Expiry::In(span) => now.saturating_add(
                span.seconds()
                    .saturating_mul(1_000)
                    .saturating_add(i64::from(span.nanos() / 1_000_000)),
            ),
            Expiry::At(instant) => instant
                .seconds()
                .saturating_mul(1_000)
                .saturating_add(i64::from(instant.nanos() / 1_000_000)),
        };
        let current = current.map(|left| now.saturating_add(left));
        let applies = match (when, current) {
            (When::NoExpiry, current) => current.is_none(),
            (When::HasExpiry, current) => current.is_some(),
            (When::Later, Some(current)) => new > current,
            (When::Sooner, Some(current)) => new < current,
            (When::Later, None) => false,
            (When::Sooner, None) => true,
        };
        if !applies {
            return Reply::Integer(0);
        }
        let mut binder = Binder::default();
        let planned = expire(key, at, self.space, &mut binder);
        self.planned(planned, binder).await
    }
}
