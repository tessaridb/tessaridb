//! A record of a transaction across leaders, carried to the leader of its
//! range (ADR-0112) — the same carriage as a coordinated request (ADR-0108):
//! an assertion signed by the asking node's key names the caller, the door
//! believes it against the certificate the connection presented, and the
//! answering node judges the user in its own catalog. No password crosses.

use std::time::Duration;

use tessari_constants::COORDINATED_SECONDS;
use tessari_encoding::NODE_ID_LEN;
use tessari_session::{AcrossAnswer, AcrossAsk, PartRefused, Participants};
use tessari_storage::UserDefinition;

use crate::assertion::{Assertion, Principal, Signed, nonce, now_ms, request_digest};
use crate::coordination::{Coordinator, account};
use crate::error::{Error, Result};
use crate::frame;
pub(crate) mod kept;

/// What an assertion carrying a record is made for: the record's bytes, under
/// a name no script can take.
pub(crate) const ACROSS: &str = "ACROSS";

/// A record, and the assertion it travels under.
#[derive(Debug, Clone, PartialEq)]
pub struct Carried {
    /// Who it is asked for, signed by the asking node.
    pub signed: Signed,
    /// The record, as [`AcrossAsk::encode`] wrote it.
    pub asked: Vec<u8>,
}

impl Carried {
    /// What the assertion must have been made for.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        request_digest(None, None, ACROSS, &self.asked)
    }

    /// The body of a [`crate::peer::PeerFrame::Across`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = self.signed.encode();
        frame::put_bytes(&mut body, &self.asked);
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] when the body does not hold a whole request.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (signed, at) = Signed::decode(body, 0)?;
        let (asked, at) = frame::take_bytes(body, at)?;
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self { signed, asked })
    }
}

impl Participants for Coordinator {
    fn ask(
        &self,
        to: [u8; NODE_ID_LEN],
        user: Option<&UserDefinition>,
        asked: &AcrossAsk,
    ) -> std::result::Result<AcrossAnswer, PartRefused> {
        // Everything short of the leader's own answer — no route, no key, a
        // link that failed or timed out — is a part not reached, which asking
        // again can get past.
        let began = std::time::Instant::now();
        let what = match asked {
            AcrossAsk::Prepare { .. } => "prepare",
            AcrossAsk::Resolve { .. } => "resolve",
            AcrossAsk::Begin { .. } | AcrossAsk::Conclude { .. } | AcrossAsk::Decide { .. } => {
                "record"
            }
            AcrossAsk::Settle { .. } => "settle",
            AcrossAsk::Lookup { .. } => "lookup",
            AcrossAsk::Holds { .. } => "holds",
            AcrossAsk::Forget { .. } => "forget",
            AcrossAsk::Bar { .. } => "bar",
        };
        let asked = asked.encode();
        let carried = Carried {
            signed: Assertion {
                from: self.me(),
                to,
                principal: user.map_or(Principal::Anonymous, |user| Principal::User {
                    id: user.id,
                    account: account(user),
                }),
                request: request_digest(None, None, ACROSS, &asked),
                nonce: nonce().map_err(|why| PartRefused::retriable(why.to_string()))?,
                issued_ms: now_ms(),
                expires_ms: now_ms().saturating_add(crate::coordination::LIFE_MILLIS),
            }
            .sign(&self.keys().duplicate().key)
            .map_err(|why| PartRefused::retriable(why.to_string()))?,
            asked,
        };
        // A link kept from an earlier record carries this one without a new
        // handshake (D13j); one the record could not be written onto is
        // replaced, and one whose answer was lost is a part not reached.
        if let Some(mut link) = self.kept().take(to) {
            match kept::across_on(&mut link, &carried) {
                Ok(reply) => {
                    self.kept().keep(to, link);
                    carried_after(what, "kept", began);
                    return replied(reply, "a kept link");
                }
                Err(kept::KeptFailed::Sent(why)) => {
                    return Err(PartRefused::retriable(format!("a kept link: {why}")));
                }
                Err(kept::KeptFailed::Unsent(why)) => {
                    tracing::debug!(error = %why, "a kept cross-leader link could not take a record");
                }
            }
        }
        let (endpoint, mine, said) = self.dialling(to).map_err(PartRefused::retriable)?;
        match kept::across_keeping(
            endpoint.as_str(),
            (self.keys(), mine),
            to,
            &said,
            (&carried, Duration::from_secs(COORDINATED_SECONDS)),
        ) {
            Ok((reply, link)) => {
                if let Some(link) = link {
                    self.kept().keep(to, link);
                }
                carried_after(what, "dialled", began);
                replied(reply, &endpoint)
            }
            Err(why) => Err(PartRefused::retriable(format!("{endpoint}: {why}"))),
        }
    }
}

/// How long a record took to be carried and answered, and on which kind of
/// link, for the per-phase attribution of a commit across leaders (Q-931).
fn carried_after(what: &'static str, link: &'static str, began: std::time::Instant) {
    let elapsed_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(what, link, elapsed_us, "a cross-leader record was carried");
}

/// The leader's answer as the coordinator takes it: its refusal keeps the kind
/// it was given and names where it came from.
fn replied(reply: kept::Reply, endpoint: &str) -> std::result::Result<AcrossAnswer, PartRefused> {
    reply.map_err(|refused| PartRefused {
        kind: refused.kind,
        reason: format!("{endpoint} refused: {}", refused.reason),
    })
}

#[cfg(test)]
mod tests;
