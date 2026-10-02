//! Peer certificates the cluster no longer accepts (ADR-0108 D6).
//!
//! A row and not a file, for the failover policy's reason: which certificates
//! are refused is a cluster-wide fact, and a list each node kept for itself
//! would be a disagreement nothing detects — the node that missed an edit keeps
//! talking to the machine everyone else refuses. A row is a log record, so the
//! revocation reaches every node by the path every other record takes.
//!
//! Keyed by the fingerprint itself, so revoking a certificate twice is one row
//! and the list needs no id or counter. The row holds the fingerprint again
//! rather than nothing, so it reads back as what it is.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{RecordId, Value};

use super::{Catalog, system};
use crate::error::{Error, Result};

const ENTITY: &str = "revoked certificate";
const FIELD_FINGERPRINT: &str = "fingerprint";

impl Catalog<'_, '_> {
    /// Refuse the certificate whose SHA-256 is `fingerprint` from now on.
    ///
    /// `fingerprint` is lowercase hexadecimal, as the statement normalises it.
    pub fn revoke_certificate(&mut self, fingerprint: &str) {
        let row = Value::Object(BTreeMap::from([(
            FIELD_FINGERPRINT.to_owned(),
            Value::String(fingerprint.to_owned()),
        )]));
        self.transaction.put(
            system::address(system::REVOKED_CERTIFICATES, RecordId::from(fingerprint)),
            encode_payload(&row).into_bytes(),
        );
    }

    /// Every revoked fingerprint, in order.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a revocation.
    pub fn revoked_certificates(&self) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::REVOKED_CERTIFICATES,
        )? {
            let row = decode_payload(&payload)?;
            match &row {
                Value::Object(fields) => match fields.get(FIELD_FINGERPRINT) {
                    Some(Value::String(fingerprint)) => found.push(fingerprint.clone()),
                    other => {
                        return Err(Error::CatalogMalformed {
                            entity: ENTITY,
                            field: FIELD_FINGERPRINT,
                            found: other.map_or("none", Value::type_name),
                        });
                    }
                },
                other => {
                    return Err(Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_FINGERPRINT,
                        found: other.type_name(),
                    });
                }
            }
        }
        Ok(found)
    }
}
