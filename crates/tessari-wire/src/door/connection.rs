use super::*;

mod pushing;

/// Everything one connection needs, owned so the task can hold it.
pub(super) struct Connection<H> {
    /// The door's own stop, so a held stream ends with the door rather than
    /// being cut at the drain deadline.
    pub(super) stop: CancellationToken,
    pub(super) acceptor: TlsAcceptor,
    pub(super) me: [u8; NODE_ID_LEN],
    pub(super) voter: Arc<Deciding>,
    pub(super) holding: Arc<H>,
    pub(super) bridge: Arc<Bridge>,
    /// The nonces of the assertions this door believed, so none is believed
    /// twice (ADR-0108 D3).
    pub(super) replays: Arc<Replays>,
    /// What a handshake is judged by, asked again while a stream is open.
    pub(super) keys: crate::PeerKeys,
}

impl<H: Holding> Connection<H> {
    /// Serve one peer, and say how it ended.
    pub(super) async fn serve(self, socket: tokio::net::TcpStream) -> Ended {
        match self.exchange(socket).await {
            // A stream recorded its peer when it opened (see `stream`).
            Ok(None) => Ended::Served,
            Ok(Some(met)) => {
                let holding = Arc::clone(&self.holding);
                if let Bridged::Busy(()) | Bridged::Panicked =
                    self.bridge.call((), move |()| holding.met(&met)).await
                {
                    tracing::warn!("a peer was served but could not be recorded");
                }
                Ended::Served
            }
            Err(why @ Error::NothingToSay(_)) => Ended::NothingToSay(why),
            // Info and not warn: a peer hanging up and a credential this
            // cluster does not issue are ordinary events on a door.
            Err(why) => {
                tracing::info!(reason = %why, "a peer connection ended");
                Ended::Served
            }
        }
    }

    /// The handshake, the greetings and the one follow-up — `greet`'s order.
    async fn exchange(&self, socket: tokio::net::TcpStream) -> Result<Option<Met>> {
        let mut link = bounded(self.acceptor.accept(socket))
            .await?
            .map_err(|why| Error::Transport(why.to_string()))?;
        let shown = link
            .get_ref()
            .1
            .peer_certificates()
            .and_then(<[_]>::first)
            .cloned();
        let Some((tag, body)) = bounded(frame_async::read_tagged(&mut link)).await?? else {
            return Err(Error::Truncated);
        };
        let said = greeting(tag, &body)?;
        let presented = credential::presented(shown.as_ref(), said.node)?;
        admit(Some(&presented), &said, &self.me)?;

        // Read now, after the peer proved itself, for the reason `greet` gives.
        let holding = Arc::clone(&self.holding);
        let mine = self.store(move || holding.hello()).await?;
        bounded(frame_async::write_tagged(
            &mut link,
            PeerFrame::Hello.tag(),
            &mine.encode(),
        ))
        .await??;

        let asked = match bounded(frame_async::read_tagged(&mut link)).await? {
            Ok(asked) => asked,
            Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(why) => return Err(why),
        };
        let voted = match asked {
            None => None,
            // A held stream (ADR-0106 D5): recorded now, because it may stay
            // open for hours and a greeting bound only at its end would leave a
            // joining follower's row unbound all that time.
            Some((tag, body))
                if tag == PeerFrame::Stream.tag() || tag == PeerFrame::StreamFrom.tag() =>
            {
                let holding = Arc::clone(&self.holding);
                let met = Met {
                    said,
                    voted: None,
                    presented: shown.as_ref().map_or([0; 32], credential::digest),
                };
                if let Bridged::Busy(()) | Bridged::Panicked =
                    self.bridge.call((), move |()| holding.met(&met)).await
                {
                    tracing::warn!("a peer opened a stream but could not be recorded");
                }
                // A follower the leader pushes to (ADR-0120) opens with its
                // own frame, and is served by the loop that reads while it waits.
                if tag == PeerFrame::StreamFrom.tag() {
                    self.stream_pushed(&mut link, said.node, body, shown.as_ref())
                        .await?;
                } else {
                    self.stream(&mut link, said.node, body, shown.as_ref())
                        .await?;
                }
                return Ok(None);
            }
            // A copy streams (ADR-0094 D3): the store side runs on the bridge
            // and hands each frame over a bounded channel, so a follower that
            // reads slowly slows the read instead of filling memory, and one
            // that goes away closes the channel and ends the read.
            Some((tag, _)) if tag == PeerFrame::State.tag() => {
                let holding = Arc::clone(&self.holding);
                let node = said.node;
                let (sender, receiver) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(4);
                let copying = self.store(move || {
                    holding.copied(node, &mut |tag, body| {
                        sender.blocking_send((tag, body)).map_err(|_| {
                            Error::Transport("the follower stopped reading the copy".to_owned())
                        })
                    })
                });
                let forwarding = async {
                    let mut receiver = receiver;
                    while let Some((tag, body)) = receiver.recv().await {
                        bounded(frame_async::write_tagged(&mut link, tag, &body)).await??;
                    }
                    Ok::<(), Error>(())
                };
                let (copied, forwarded) = tokio::join!(copying, forwarding);
                forwarded?;
                match copied {
                    Ok(()) => {}
                    Err(Error::Unsubscribed) => {
                        bounded(frame_async::write_tagged(
                            &mut link,
                            PeerFrame::Unsubscribed.tag(),
                            &[],
                        ))
                        .await??;
                    }
                    Err(why) => return Err(why),
                }
                None
            }
            // A request carried here for a caller (ADR-0108 D1–D3). Believed
            // against the certificate THIS handshake proved, so the signer is
            // the peer on this connection and no other.
            // A record of a transaction across leaders, carried here as a
            // coordinated request is (ADR-0112) and believed by the same rule.
            Some((tag, body)) if tag == PeerFrame::Across.tag() => {
                let shown = shown.as_ref().ok_or(Error::Unidentified)?;
                self.across(&mut link, shown, &said, &body).await?;
                // A coordinator on a build that keeps links writes its next
                // record straight onto this one (ADR-0112 D13j).
                if said.build >= crate::across::kept::KEPT_FROM {
                    self.keep_across(&mut link, shown, &said).await?;
                }
                None
            }
            Some((tag, body)) if tag == PeerFrame::Coordinate.tag() => {
                let shown = shown.as_ref().ok_or(Error::Unidentified)?;
                let asked = Coordinate::decode(&body)?;
                let believed = asked
                    .signed
                    .verify(
                        shown,
                        said.node,
                        self.me,
                        (asked.digest(), now_ms()),
                        &self.replays,
                    )
                    .copied();
                let (tag, reply) = match believed {
                    Err(why) => {
                        tracing::warn!(
                            peer = %tessari_types::uuid_to_text(&said.node),
                            refusal = %why,
                            "a carried request was refused"
                        );
                        (
                            PeerFrame::NotCoordinated.tag(),
                            why.to_string().into_bytes(),
                        )
                    }
                    Ok(assertion) => {
                        tracing::info!(
                            peer = %tessari_types::uuid_to_text(&said.node),
                            acts_for = %match assertion.principal {
                                crate::assertion::Principal::Anonymous => "nobody".to_owned(),
                                crate::assertion::Principal::User { id, .. } =>
                                    format!("user {id}"),
                            },
                            nonce = %hex(&assertion.nonce),
                            "a carried request was accepted"
                        );
                        let holding = Arc::clone(&self.holding);
                        let from = said.node;
                        let answered = self
                            .store(move || Ok(holding.coordinated(from, &assertion, &asked)))
                            .await?;
                        match answered {
                            Ok(answer) => (PeerFrame::Coordinated.tag(), encode_answer(&answer)),
                            Err(why) => (PeerFrame::NotCoordinated.tag(), why.into_bytes()),
                        }
                    }
                };
                bounded(frame_async::write_tagged(&mut link, tag, &reply)).await??;
                None
            }
            Some((tag, body)) => {
                let holding = Arc::clone(&self.holding);
                let voter = Arc::clone(&self.voter);
                let (tag, reply, voted) = self
                    .store(move || answering(tag, &body, &said, &mine, &voter, &*holding))
                    .await?;
                bounded(frame_async::write_tagged(&mut link, tag, &reply)).await??;
                voted
            }
        };
        Ok(Some(Met {
            said,
            voted,
            presented: shown.as_ref().map_or([0; 32], credential::digest),
        }))
    }

    /// Answer one carried record of a transaction across leaders, believed
    /// against the certificate this handshake proved — so the signer is the
    /// peer on this connection and no other (ADR-0112, ADR-0108 D1–D3).
    async fn across(
        &self,
        link: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        shown: &rustls::pki_types::CertificateDer<'_>,
        said: &Hello,
        body: &[u8],
    ) -> Result<()> {
        let carried = crate::across::Carried::decode(body)?;
        let believed = carried
            .signed
            .verify(
                shown,
                said.node,
                self.me,
                (carried.digest(), now_ms()),
                &self.replays,
            )
            .copied();
        let (tag, reply) = match believed {
            Err(why) => {
                tracing::warn!(
                    peer = %tessari_types::uuid_to_text(&said.node),
                    refusal = %why,
                    "a carried cross-leader record was refused"
                );
                // This node will not act for the caller as asserted,
                // and asking again changes nothing.
                let refused = tessari_session::PartRefused {
                    kind: tessari_session::RefusalKind::Forbidden,
                    reason: why.to_string(),
                };
                (PeerFrame::NotAcross.tag(), refused.encode())
            }
            Ok(assertion) => {
                let holding = Arc::clone(&self.holding);
                let from = said.node;
                let answered = self
                    .store(move || Ok(holding.across(from, &assertion, &carried.asked)))
                    .await?;
                match answered {
                    Ok(answer) => (PeerFrame::AcrossDone.tag(), answer),
                    Err(refused) => (PeerFrame::NotAcross.tag(), refused.encode()),
                }
            }
        };
        bounded(frame_async::write_tagged(link, tag, &reply)).await??;
        Ok(())
    }

    /// Answer every further record the coordinator writes onto this link,
    /// until it goes quiet for [`ACROSS_DOOR_IDLE_SECONDS`], closes it, or the
    /// door stops (ADR-0112 D13j).
    ///
    /// Each record is believed by its own signed assertion, as the first was,
    /// and the certificate the link proved is asked about again before each —
    /// a node revoked or removed while its link is kept is cut at its next
    /// record, as a held stream is (ADR-0108 D6). Anything but a record ends
    /// the link: it is kept for this and nothing else.
    async fn keep_across(
        &self,
        link: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        shown: &rustls::pki_types::CertificateDer<'_>,
        said: &Hello,
    ) -> Result<()> {
        let idle = std::time::Duration::from_secs(ACROSS_DOOR_IDLE_SECONDS);
        loop {
            let next = tokio::select! {
                biased;
                () = self.stop.cancelled() => return Ok(()),
                next = tokio::time::timeout(idle, frame_async::read_tagged(link)) => next,
            };
            let body = match next {
                // Quiet past the coordinator's own limit: it will not use it.
                Err(_) => return Ok(()),
                Ok(Ok(Some((tag, body)))) if tag == PeerFrame::Across.tag() => body,
                Ok(Ok(Some((tag, _)))) => return Err(Error::OutOfTurn { tag }),
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(Error::Io(why))) if why.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Ok(Err(why)) => return Err(why),
            };
            if !self.keys.still_admits(shown) {
                tracing::warn!(
                    peer = %tessari_types::uuid_to_text(&said.node),
                    "a kept cross-leader link ended: its certificate is no longer admitted here"
                );
                return Ok(());
            }
            self.across(link, shown, said, &body).await?;
        }
    }

    /// Serve a held stream (ADR-0106 D5) until the follower closes or the door
    /// stops.
    ///
    /// Each ask is answered by exactly ONE round carrying records. While there
    /// is nothing to send the leader reads nothing: it waits on its commit
    /// signal and, every [`STREAM_HEARTBEAT_MILLIS`], sends an empty round — the
    /// heartbeat — which says *nothing after your positions has landed here*.
    /// That claim is exact rather than hopeful because the signal is marked
    /// seen before every read of the log, so a commit landing during a read
    /// wakes the next one instead of being slept past.
    async fn stream(
        &self,
        link: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        follower: [u8; NODE_ID_LEN],
        mut body: Vec<u8>,
        presented: Option<&rustls::pki_types::CertificateDer<'_>>,
    ) -> Result<()> {
        let heartbeat = std::time::Duration::from_millis(STREAM_HEARTBEAT_MILLIS);
        // A stream outlives its handshake by hours, so the handshake's judgement
        // is asked again before every frame: a certificate revoked, or a node
        // removed, while it streams is cut off at the next one (ADR-0108 D6).
        let admitted = || presented.is_some_and(|presented| self.keys.still_admits(presented));
        let quiet = crate::collection::Streamed {
            answers: Vec::new(),
        }
        .encode();
        let mut commits = self.holding.commits();
        loop {
            let asked = StreamAsk::decode(&body)?;
            let round = loop {
                if !admitted() {
                    tracing::warn!(
                        "a held stream ended: the peer's certificate is no longer admitted here"
                    );
                    return Ok(());
                }
                commits.borrow_and_update();
                let holding = Arc::clone(&self.holding);
                let ask = asked.clone();
                let answered = self
                    .store(move || stream_answer(&*holding, follower, &ask))
                    .await;
                let round = match answered {
                    Ok(round) => round,
                    // The refusal crosses as the frame a round would carry, and
                    // the stream ends: the follower's round meets it again,
                    // which is where every repair already lives.
                    Err(Error::Uncollectable { from }) => {
                        let mut refused = Vec::with_capacity(8);
                        crate::frame::put_u64(&mut refused, from);
                        bounded(frame_async::write_tagged(
                            link,
                            PeerFrame::Uncollectable.tag(),
                            &refused,
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(Error::Unsubscribed) => {
                        bounded(frame_async::write_tagged(
                            link,
                            PeerFrame::Unsubscribed.tag(),
                            &[],
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(why) => return Err(why),
                };
                if !round.is_quiet() {
                    break round;
                }
                // Nothing to send: wait for a commit, saying so every beat.
                loop {
                    tokio::select! {
                        biased;
                        () = self.stop.cancelled() => return Ok(()),
                        changed = commits.changed() => {
                            if changed.is_err() {
                                // The commit signal went with the node.
                                return Ok(());
                            }
                            break;
                        }
                        () = tokio::time::sleep(heartbeat) => {
                            if !admitted() {
                                tracing::warn!(
                                    "a held stream ended: the peer's certificate is no longer admitted here"
                                );
                                return Ok(());
                            }
                            bounded(frame_async::write_tagged(
                                link,
                                PeerFrame::Streamed.tag(),
                                &quiet,
                            ))
                            .await??;
                        }
                    }
                }
            };
            bounded(frame_async::write_tagged(
                link,
                PeerFrame::Streamed.tag(),
                &round.encode(),
            ))
            .await??;
            let next = tokio::select! {
                biased;
                () = self.stop.cancelled() => return Ok(()),
                next = bounded(frame_async::read_tagged(link)) => next?,
            };
            body = match next {
                Ok(Some((tag, next))) if tag == PeerFrame::Stream.tag() => next,
                Ok(Some((tag, _))) => return Err(Error::OutOfTurn { tag }),
                Ok(None) => return Ok(()),
                Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Err(why) => return Err(why),
            };
        }
    }

    /// Run a store call on the bridge, and read a refusal as the error it is.
    async fn store<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match self.bridge.call((), move |()| work()).await {
            Bridged::Answered(answer) => answer,
            Bridged::Busy(()) => Err(Error::Transport(
                "the peer door is serving as many store calls as it will".to_owned(),
            )),
            Bridged::Panicked => Err(Error::Transport(
                "the store call for this peer panicked".to_owned(),
            )),
        }
    }
}
