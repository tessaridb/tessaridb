//! A connection that has become a subscription: what it is sent, and when.

use super::{BUSY, Conversation};
use crate::error::{Error, Result};
use crate::push::Follow;
use crate::{READING, frame, frame_async, push};
use std::sync::Arc;
use tessari_serve::Bridged;
use tessari_session::Detached;
use tessaridb::Sequence;
use tessaridb::feed::{self, Feed, Following, Round};
use tokio::io::{AsyncReadExt, BufReader, BufWriter as AsyncBufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Push changes down this connection until it ends — as a task, not a thread.
///
/// Opening and every round cross the bridge like a statement, because both read
/// the store. Between rounds the task waits on three things and holds nothing:
/// the store's announcement that something landed ([`tessaridb::Db::commits`]),
/// the socket, and [`feed::PATIENCE_BETWEEN_ROUNDS`] — the last only so that a
/// shutdown is seen; a round runs when something landed and at no other time.
/// The socket is where a subscriber that hung up on a quiet feed is noticed
/// (F-S1): reading end-of-stream ends the feed, with no write needed to learn it.
pub(crate) async fn feed(
    talk: Conversation,
    session: Detached,
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: AsyncBufWriter<OwnedWriteHalf>,
    asked: Follow,
) -> Result<()> {
    let db = Arc::clone(&talk.db);
    let opened = talk
        .bridge
        .call(session, move |held: Detached| {
            let mut attached = held.attach(db.store());
            let following = Following {
                from: Sequence::new(asked.from),
                table: asked.table.as_deref(),
                cursor: asked.cursor.as_deref(),
            };
            let opened = Feed::open(&db, &mut attached, &following);
            (attached.detach(), opened)
        })
        .await;
    let (mut session, mut following) = match opened {
        Bridged::Answered((back, Ok(opened))) => (back, opened),
        // The store's own words, travelling as a refusal: every one of them is a
        // state the subscriber can correct.
        Bridged::Answered((_, Err(refusal))) => {
            return frame_async::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )
            .await;
        }
        Bridged::Busy(_) => {
            return frame_async::write(&mut writer, frame::Kind::Refusal, BUSY.as_bytes()).await;
        }
        Bridged::Panicked => {
            return Err(Error::Io(std::io::Error::other(
                "opening the feed panicked",
            )));
        }
    };
    let mut commits = talk.committed.watching();
    let mut stray = [0_u8; 64];
    // Whether the log may hold something this feed has not read: at the start,
    // and after every landing the store announces. An idle feed waits without
    // touching the store (Q-838).
    let mut due = true;
    loop {
        // A staged shutdown reaches a feed here, within one wait: the cursor is a
        // position the subscriber holds, so it resumes exactly where it stopped.
        if talk.stopping.asked() {
            return Ok(());
        }
        if due {
            // Marked seen BEFORE the round, so a landing during it wakes the wait.
            commits.borrow_and_update();
            let db = Arc::clone(&talk.db);
            let ran = talk
                .rounds
                .call(
                    (session, following),
                    move |(held, mut open): (Detached, Feed)| {
                        let mut attached = held.attach(db.store());
                        let mut frames = Vec::new();
                        let round =
                            open.round(&db, &mut attached, &mut |change, name, allowed, cursor| {
                                // A change whose table has been dropped has no name to give.
                                if let Some(named) =
                                    push::named(change, name.map(str::to_owned), cursor)
                                {
                                    frames.push(named.hiding(allowed).encode());
                                }
                                true
                            });
                        ((attached.detach(), open), round, frames)
                    },
                )
                .await;
            let round = match ran {
                Bridged::Answered(((back, open), round, frames)) => {
                    (session, following) = (back, open);
                    due = false;
                    for change in &frames {
                        // A subscriber that stops reading ends its own feed rather
                        // than holding its place until the process stops.
                        tokio::time::timeout(
                            READING,
                            frame_async::write(&mut writer, frame::Kind::Change, change),
                        )
                        .await
                        .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))??;
                    }
                    round
                }
                // Every round slot is taken: this feed stays due and tries again after
                // the next wait. A busy node delays a feed; it does not refuse a
                // subscriber who was already admitted.
                Bridged::Busy((back, open)) => {
                    (session, following) = (back, open);
                    Ok(Round::Empty)
                }
                Bridged::Panicked => {
                    return Err(Error::Io(std::io::Error::other("a feed round panicked")));
                }
            };
            match round {
                // A mouthful: there may be more, so look again at once.
                Ok(Round::Delivered) => {
                    due = true;
                    continue;
                }
                Ok(Round::Empty | Round::Ended) => {}
                Err(refusal) => {
                    return frame_async::write(
                        &mut writer,
                        frame::Kind::Refusal,
                        refusal.to_string().as_bytes(),
                    )
                    .await;
                }
            }
        }
        tokio::select! {
            biased;
            read = reader.read(&mut stray) => match read {
                // End of stream, or a socket error: the subscriber has gone.
                Ok(0) | Err(_) => return Ok(()),
                // Bytes mean the subscriber is still there; a feed carries
                // nothing in that direction, so they are not answered.
                Ok(_) => {}
            },
            _ = commits.changed() => due = true,
            () = tokio::time::sleep(feed::PATIENCE_BETWEEN_ROUNDS) => {}
        }
    }
}
