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
use crate::link::{Answered, Ask, call_within};

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
        let (endpoint, mine, said) = self.dialling(to).map_err(PartRefused::retriable)?;
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
            .sign(&mine.key)
            .map_err(|why| PartRefused::retriable(why.to_string()))?,
            asked,
        };
        match call_within(
            endpoint.as_str(),
            (self.keys(), mine),
            to,
            &said,
            Ask::Across(&carried),
            Duration::from_secs(COORDINATED_SECONDS),
        ) {
            Ok((_, Answered::Across(answer))) => Ok(answer),
            Ok(_) => Err(PartRefused::retriable(format!(
                "{endpoint} answered something other than the record"
            ))),
            Err(Error::RefusedAcross(refused)) => Err(PartRefused {
                kind: refused.kind,
                reason: format!("{endpoint} refused: {}", refused.reason),
            }),
            Err(why) => Err(PartRefused::retriable(format!("{endpoint}: {why}"))),
        }
    }
}

#[cfg(test)]
mod tests;
