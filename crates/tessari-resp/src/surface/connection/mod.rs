//! One RESP connection: commands in, replies out, in the order asked.
//!
//! A client may pipeline — send many commands before reading a reply — so
//! every whole command buffered is answered before the replies are written,
//! and they are written together: one write for a pipeline, not one per
//! command. Commands on a connection are answered one at a time, in order, as
//! the protocol requires; concurrency is across connections, each its own task.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::{
    FRAME_STALL_SECONDS, RESP_IDLE_SECONDS, RESP_MULTI_COMMANDS, RESP_PIPELINE_COMMANDS,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use zeroize::Zeroize;

use crate::codec::{Command, Decoder, Protocol, Reply, encode};
use crate::surface::commands::{self, Context, Procedure, Step};
use crate::surface::procedures::{Against, Cursors};
use crate::surface::replies;
use crate::surface::running::Held;
use crate::surface::statement::{Binder, Planned};
use crate::surface::{Shared, Space};

mod blocked;
mod introduce;
mod watch;

/// How often an idle connection looks at whether the node is stopping.
const WATCH_STOPPING: Duration = Duration::from_secs(1);

/// How much is read from the socket at a time.
const READ_CHUNK: usize = 16 * 1024;

/// Whether the connection goes on after a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

/// A command queued after `MULTI`.
enum Queued {
    Reply(Reply),
    Planned(Planned),
}

/// What `MULTI` has queued for its `EXEC`.
#[derive(Default)]
struct Queue {
    binder: Binder,
    commands: Vec<Queued>,
    /// A command could not be queued, so `EXEC` runs nothing.
    failed: bool,
}

/// One connection's state between commands.
struct Connection {
    shared: Arc<Shared>,
    held: Held,
    protocol: Protocol,
    space: Option<Space>,
    name: Option<Vec<u8>>,
    user: Option<String>,
    id: u64,
    peer: Option<SocketAddr>,
    multi: Option<Queue>,
    /// What each `WATCH`ed key held, for the next `EXEC`'s guards.
    watched: Vec<(Vec<u8>, tessaridb::Value)>,
    cursors: Cursors,
    /// A blocking pop that found nothing yet, for the connection to wait on.
    blocked: Option<blocked::Blocked>,
}

/// Serve one connection until the client closes it, it idles out, a command
/// stalls half-sent, or the node stops.
pub(crate) async fn converse<S>(
    shared: Arc<Shared>,
    peer: Option<SocketAddr>,
    mut stream: S,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut connection = Connection {
        held: Held::anonymous(&shared),
        protocol: Protocol::Two,
        space: shared.space.clone(),
        name: None,
        user: None,
        id: shared.next_client(),
        peer,
        multi: None,
        watched: Vec::new(),
        cursors: Cursors::default(),
        blocked: None,
        shared,
    };
    let mut decoder = Decoder::new();
    let mut chunk = vec![0_u8; READ_CHUNK];
    let mut out = Vec::with_capacity(READ_CHUNK);
    loop {
        let mut answered = 0_usize;
        loop {
            let command = match decoder.command() {
                Ok(Some(command)) => command,
                Ok(None) => break,
                Err(why) => {
                    encode(
                        &Reply::error(format!("ERR {why}")),
                        connection.protocol,
                        &mut out,
                    );
                    stream.write_all(&out).await?;
                    stream.flush().await?;
                    tracing::info!(reason = %why, "resp connection closed: unreadable bytes");
                    return Ok(());
                }
            };
            let busy = connection.shared.stopping.busy();
            let (mut reply, flow) = connection.handle(command).await;
            drop(busy);
            if let Some(blocked) = connection.blocked.take() {
                // The replies before it are the client's to read while it waits.
                if !out.is_empty() {
                    stream.write_all(&out).await?;
                    stream.flush().await?;
                    out.clear();
                }
                match connection
                    .wait(blocked, &mut stream, &mut decoder, &mut chunk)
                    .await?
                {
                    Some(waited) => reply = waited,
                    None => return Ok(()),
                }
            }
            connection
                .shared
                .stopping
                .answered(matches!(reply, Reply::Error(_)));
            encode(&reply, connection.protocol, &mut out);
            if flow == Flow::Close {
                stream.write_all(&out).await?;
                stream.flush().await?;
                return Ok(());
            }
            answered = answered.saturating_add(1);
            if answered >= RESP_PIPELINE_COMMANDS {
                // A long pipeline is answered in rounds, so its replies do not
                // pile up here and the worker is handed back between them.
                break;
            }
        }
        if !out.is_empty() {
            stream.write_all(&out).await?;
            stream.flush().await?;
            out.clear();
        }
        if answered >= RESP_PIPELINE_COMMANDS {
            tokio::task::yield_now().await;
            continue;
        }
        let Some(read) = next_bytes(&connection.shared, &decoder, &mut stream, &mut chunk).await?
        else {
            return Ok(());
        };
        decoder.extend(&chunk[..read]);
    }
}

/// The next bytes, or `None` when the connection ended, idled out, stalled
/// half-way through a command, or the node is stopping.
async fn next_bytes<S: AsyncRead + Unpin>(
    shared: &Shared,
    decoder: &Decoder,
    stream: &mut S,
    chunk: &mut [u8],
) -> std::io::Result<Option<usize>> {
    let mut idle = Duration::ZERO;
    loop {
        match tokio::time::timeout(WATCH_STOPPING, stream.read(chunk)).await {
            Ok(Ok(0)) => return Ok(None),
            Ok(Ok(read)) => return Ok(Some(read)),
            Ok(Err(why)) => return Err(why),
            Err(_) => {
                idle = idle.saturating_add(WATCH_STOPPING);
                let patience = if decoder.holds_a_part() {
                    FRAME_STALL_SECONDS
                } else {
                    RESP_IDLE_SECONDS
                };
                if shared.stopping.asked() || idle >= Duration::from_secs(patience) {
                    return Ok(None);
                }
            }
        }
    }
}

impl Connection {
    async fn handle(&mut self, mut command: Command) -> (Reply, Flow) {
        // `AUTH` and `HELLO … AUTH` carry a password; their arguments are
        // wiped once answered, as the Kafka surface wipes a SASL frame.
        let secret = command.first().is_some_and(|name| {
            name.eq_ignore_ascii_case(b"AUTH") || name.eq_ignore_ascii_case(b"HELLO")
        });
        let answered = self.answer(&command).await;
        if secret {
            command.iter_mut().for_each(Zeroize::zeroize);
        }
        answered
    }

    async fn answer(&mut self, command: &Command) -> (Reply, Flow) {
        let mut fresh = Binder::default();
        let context = Context {
            space: self.space.as_ref(),
        };
        let binder = match self.multi.as_mut() {
            Some(queue) => &mut queue.binder,
            None => &mut fresh,
        };
        let planned = commands::plan(command, &context, binder);
        if self.multi.is_some() {
            return self.queue(command, planned).await;
        }
        let step = match planned {
            Ok(step) => step,
            Err(reply) => return (reply, Flow::Continue),
        };
        match step {
            Step::Reply(reply) => (reply, Flow::Continue),
            Step::Statements(planned) => match self.against() {
                Ok(mut against) => (against.planned(planned, fresh).await, Flow::Continue),
                Err(reply) => (reply, Flow::Continue),
            },
            Step::Procedure(procedure) => self.procedure(procedure).await,
        }
    }

    fn against(&mut self) -> Result<Against<'_>, Reply> {
        let space = self.space.as_ref().ok_or_else(replies::no_space)?;
        Ok(Against {
            shared: &self.shared,
            held: &mut self.held,
            space,
        })
    }

    /// A command after `MULTI`: queued, or the end of the queue.
    async fn queue(&mut self, command: &Command, planned: Result<Step, Reply>) -> (Reply, Flow) {
        let Some(queue) = self.multi.as_mut() else {
            return (replies::syntax(), Flow::Continue);
        };
        let queued = match planned {
            Ok(Step::Procedure(Procedure::Exec)) => return (self.exec().await, Flow::Continue),
            Ok(Step::Procedure(Procedure::Discard)) => {
                self.multi = None;
                self.watched.clear();
                return (Reply::OK, Flow::Continue);
            }
            Ok(Step::Procedure(Procedure::Watch(_))) => {
                return (
                    Reply::error("ERR WATCH inside MULTI is not allowed"),
                    Flow::Continue,
                );
            }
            // `EXEC` forgets what was watched anyway.
            Ok(Step::Procedure(Procedure::Unwatch)) => Queued::Reply(Reply::OK),
            Ok(Step::Procedure(Procedure::Multi)) => {
                return (
                    Reply::error("ERR MULTI calls can not be nested"),
                    Flow::Continue,
                );
            }
            Ok(Step::Procedure(Procedure::Quit)) => return (Reply::OK, Flow::Close),
            Ok(Step::Reply(reply)) => Queued::Reply(reply),
            Ok(Step::Statements(planned)) => Queued::Planned(planned),
            // Inside MULTI a blocking pop does not block: it is its form, when
            // that is a statement.
            Ok(Step::Procedure(Procedure::Block { form, .. })) => {
                let context = Context {
                    space: self.space.as_ref(),
                };
                match commands::plan(&form, &context, &mut queue.binder) {
                    Ok(Step::Statements(planned)) => Queued::Planned(planned),
                    _ => {
                        queue.failed = true;
                        return (not_in_multi(command), Flow::Continue);
                    }
                }
            }
            Ok(Step::Procedure(_)) => {
                queue.failed = true;
                return (not_in_multi(command), Flow::Continue);
            }
            Err(reply) => {
                queue.failed = true;
                return (reply, Flow::Continue);
            }
        };
        if queue.commands.len() >= RESP_MULTI_COMMANDS {
            queue.failed = true;
            return (
                Reply::error(format!(
                    "ERR a MULTI holds at most {RESP_MULTI_COMMANDS} commands"
                )),
                Flow::Continue,
            );
        }
        queue.commands.push(queued);
        (Reply::Status("QUEUED".into()), Flow::Continue)
    }

    /// `EXEC`: every queued command's statements in one transaction.
    async fn exec(&mut self) -> Reply {
        let Some(mut queue) = self.multi.take() else {
            return Reply::error("ERR EXEC without MULTI");
        };
        let watched = std::mem::take(&mut self.watched);
        if queue.failed {
            return Reply::error("EXECABORT Transaction discarded because of previous errors.");
        }
        let guards = match self.space.as_ref() {
            Some(space) => watch::guards(watched, space, &mut queue.binder),
            None => Vec::new(),
        };
        let guarded = guards.len();
        let statements: Vec<String> = guards
            .into_iter()
            .chain(
                queue
                    .commands
                    .iter()
                    .filter_map(|queued| match queued {
                        Queued::Planned(planned) => Some(planned.statements.clone()),
                        Queued::Reply(_) => None,
                    })
                    .flatten(),
            )
            .collect();
        let mut outcomes = if statements.is_empty() {
            Vec::new()
        } else {
            // One script, one transaction when there is more than one
            // statement; a single statement is atomic on its own.
            let parameters = queue.binder.into_parameters();
            let mut against = match self.against() {
                Ok(against) => against,
                Err(reply) => return reply,
            };
            match against.run(&statements, &parameters).await {
                Ok(outcomes) => outcomes,
                // A watched key changed: the transaction did not run.
                Err(failed) if error_text(&replies::failed(&failed)).contains(watch::WATCHED) => {
                    return Reply::NilArray;
                }
                Err(failed) => {
                    return Reply::error(format!(
                        "EXECABORT Transaction discarded: {}",
                        error_text(&replies::failed(&failed))
                    ));
                }
            }
        }
        .into_iter();
        // The guards answer nothing of their own.
        outcomes.by_ref().take(guarded).for_each(drop);
        let replies = queue
            .commands
            .into_iter()
            .map(|queued| match queued {
                Queued::Reply(reply) => reply,
                Queued::Planned(planned) => {
                    let own = outcomes.by_ref().take(planned.statements.len()).collect();
                    (planned.decode)(own)
                }
            })
            .collect();
        Reply::Array(replies)
    }

    async fn procedure(&mut self, procedure: Procedure) -> (Reply, Flow) {
        let reply = match procedure {
            Procedure::Quit => return (Reply::OK, Flow::Close),
            Procedure::Reset => {
                self.protocol = Protocol::Two;
                self.multi = None;
                self.watched.clear();
                self.name = None;
                self.user = None;
                self.space = self.shared.space.clone();
                self.cursors = Cursors::default();
                self.held.forget(&self.shared);
                Reply::Status("RESET".into())
            }
            Procedure::Auth { user, mut password } => {
                let reply = self.auth(user.as_deref(), &password).await;
                password.zeroize();
                reply
            }
            Procedure::Hello {
                protocol,
                auth,
                name,
            } => self.hello(protocol, auth, name).await,
            Procedure::Client(arguments) => self.client(&arguments),
            Procedure::Use(space) => {
                self.space = Some(space);
                self.cursors = Cursors::default();
                Reply::OK
            }
            Procedure::Multi => {
                self.multi = Some(Queue::default());
                Reply::OK
            }
            Procedure::Exec => Reply::error("ERR EXEC without MULTI"),
            Procedure::Watch(keys) => self.watch(keys).await,
            Procedure::Unwatch => {
                self.watched.clear();
                Reply::OK
            }
            Procedure::Discard => Reply::error("ERR DISCARD without MULTI"),
            Procedure::Block { form, wait, shape } => self.block(form, wait, shape).await,
            store => {
                let Some(space) = self.space.as_ref() else {
                    return (replies::no_space(), Flow::Continue);
                };
                let mut against = Against {
                    shared: &self.shared,
                    held: &mut self.held,
                    space,
                };
                match store {
                    Procedure::Keys(pattern) => against.keys(&pattern).await,
                    Procedure::Scan {
                        cursor,
                        pattern,
                        count,
                        kind,
                    } => {
                        against
                            .scan(
                                &mut self.cursors,
                                cursor,
                                (pattern.as_deref(), kind.as_deref()),
                                count,
                            )
                            .await
                    }
                    Procedure::RandomKey => against.first_key().await,
                    Procedure::Change { key, change } => against.change(&key, &change).await,
                    Procedure::SetIfNoneExist(pairs) => against.set_if_none_exist(&pairs).await,
                    Procedure::Rewrite { keys, compute } => against.rewrite(&keys, &compute).await,
                    Procedure::Rename { from, to, only_new } => {
                        against.rename(&from, &to, only_new).await
                    }
                    Procedure::Copy { from, to, replace } => {
                        against.copy(&from, &to, replace).await
                    }
                    Procedure::ExpireWhen { key, at, when } => {
                        against.expire_when(&key, at, when).await
                    }
                    _ => replies::syntax(),
                }
            }
        };
        (reply, Flow::Continue)
    }
}

/// The refusal of a command `MULTI` cannot queue.
fn not_in_multi(command: &Command) -> Reply {
    let name = command
        .first()
        .map(|name| String::from_utf8_lossy(name).to_ascii_lowercase())
        .unwrap_or_default();
    Reply::error(format!(
        "ERR '{name}' is not offered inside MULTI on this node"
    ))
}

fn error_text(reply: &Reply) -> String {
    match reply {
        Reply::Error(text) => text.to_string(),
        _ => String::new(),
    }
}
