//! A stream command on a connection: carried out against the topic, and an
//! `XREAD … BLOCK` waited out as a blocking pop is — its non-blocking form
//! tried again on every commit (Q-G079-10).

use super::Connection;
use crate::codec::{Command, Reply};
use crate::surface::commands::{Group, Id, Pending, Shape, Since, Stream, Wanted};
use crate::surface::procedures::Against;
use crate::surface::replies;

impl Connection {
    pub(super) async fn stream(&mut self, stream: Stream) -> Reply {
        let Some(space) = self.space.as_ref() else {
            return replies::no_space();
        };
        let mut against = Against {
            shared: &self.shared,
            held: &mut self.held,
            space,
        };
        let Stream::Read {
            count,
            keys,
            block: Some(wait),
        } = stream
        else {
            return against.stream(&stream).await;
        };
        // `$` is the last entry when the command arrived, so it is settled
        // before the wait: a later try must not move it past what was added.
        let mut ids = Vec::with_capacity(keys.len());
        for (key, since) in &keys {
            let id = match since {
                Since::Newest => match against.newest(key).await {
                    Ok(id) => id.text(),
                    Err(reply) => return reply,
                },
                Since::Last => "+".to_owned(),
                Since::After(Id { ms, seq }) => format!("{ms}-{seq}"),
            };
            ids.push(id.into_bytes());
        }
        let mut form: Command = vec![b"XREAD".to_vec()];
        if let Some(count) = count {
            form.push(b"COUNT".to_vec());
            form.push(count.to_string().into_bytes());
        }
        form.push(b"STREAMS".to_vec());
        form.extend(keys.into_iter().map(|(key, _)| key.into_bytes()));
        form.extend(ids);
        self.block(form, wait, Shape::AsIs).await
    }

    pub(super) async fn group(&mut self, group: Group) -> Reply {
        let Some(space) = self.space.as_ref() else {
            return replies::no_space();
        };
        let mut against = Against {
            shared: &self.shared,
            held: &mut self.held,
            space,
        };
        // Only a read of new entries waits; a member's own held entries are
        // there or not.
        let Group::Read {
            group: name,
            consumer,
            count,
            block: Some(wait),
            settle,
            keys,
        } = group
        else {
            return against.group(&group).await;
        };
        if keys.iter().any(|(_, wanted)| *wanted != Wanted::New) {
            return against
                .group(&Group::Read {
                    group: name,
                    consumer,
                    count,
                    block: None,
                    settle,
                    keys,
                })
                .await;
        }
        let mut form: Command = vec![
            b"XREADGROUP".to_vec(),
            b"GROUP".to_vec(),
            name.into_bytes(),
            consumer.into_bytes(),
        ];
        if let Some(count) = count {
            form.push(b"COUNT".to_vec());
            form.push(count.to_string().into_bytes());
        }
        if settle {
            form.push(b"NOACK".to_vec());
        }
        form.push(b"STREAMS".to_vec());
        let ends = vec![b">".to_vec(); keys.len()];
        form.extend(keys.into_iter().map(|(key, _)| key.into_bytes()));
        form.extend(ends);
        self.block(form, wait, Shape::AsIs).await
    }

    pub(super) async fn pending(&mut self, pending: Pending) -> Reply {
        let Some(space) = self.space.as_ref() else {
            return replies::no_space();
        };
        Against {
            shared: &self.shared,
            held: &mut self.held,
            space,
        }
        .pending(&pending)
        .await
    }
}
