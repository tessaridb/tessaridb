//! Sealed values for TessariDB: the envelope, the key hierarchy, and key
//! material that erases itself.
//!
//! # What this crate is, and where it sits
//!
//! A vault's plaintext must be unreachable **by construction, not by
//! discipline** — a rule that says "do not read from a vault" holds until the
//! next feature is written by somebody who never read it. This crate is how that
//! is arranged: it turns a value into an opaque, self-describing, authenticated
//! blob *before* the record is encoded, and it is the only code that can turn
//! one back.
//!
//! The placement is the whole design and it was chosen by measurement rather
//! than taste. A survey of this engine's own tree found that the index writer,
//! the change feed, the replication log and the backup all consume the stored
//! **payload**, below the session layer where redaction lives — and that the
//! backup *is* the log, because every derived structure in this store is a pure
//! function of it. So encrypting at the session layer would have left plaintext
//! in the search index, on every replica and in every backup file, while every
//! read through a session looked correct.
//!
//! Sealing at the payload boundary closes all four at once, and closes them
//! structurally: a writer below this crate decodes a record and finds
//! [`Value::Bytes`]-shaped opacity with nothing to leak.
//!
//! # What this crate does not claim
//!
//! It does not claim the database cannot read your secrets. While the store is
//! unsealed the master key is in memory and an authorised caller can open a
//! value — that is a deliberate decision, taken so that a command line and an
//! API can show a password without shipping cryptography to every client. The
//! claim that *is* made, and that the tests assert:
//!
//! - the stored bytes are never plaintext;
//! - no key that opens them is ever written to disk unwrapped;
//! - a sealed or restarted node cannot open anything, including for its
//!   operator.
//!
//! What that leaves exposed is written down rather than implied: the memory of a
//! running unsealed node, privileged code on the host, the operator who performs
//! the unseal, a client already holding a valid credential, and everything the
//! key layout reveals without decrypting — how many items a vault holds, how
//! large each is, when each changed, and what each is called.
//!
//! # The shape of a use
//!
//! ```no_run
//! use tessari_vault::{Binding, Keyring, Level, Root, envelope, keys};
//!
//! // Once, when the store is created. `root` is persisted; `master` is not.
//! let (root, master) = Root::create("the operator's passphrase")?;
//! let mut keyring = Keyring::sealed();
//! keyring.adopt(master)?;
//!
//! // Once per vault.
//! let (wrapped_vault, vault_key) = keys::wrap_fresh(keyring.master()?, Level::Vault, b"team")?;
//!
//! // Once per record.
//! let (wrapped_data, data_key) = keys::wrap_fresh(&vault_key, Level::Data, b"team:github")?;
//!
//! // Per secret field, bound to the place it belongs.
//! let sealed = envelope::seal(
//!     &data_key,
//!     wrapped_data.key_id,
//!     &Binding::Field { table: 7, record: b"github", field: "password" },
//!     b"hunter2",
//! )?;
//! # Ok::<(), tessari_vault::Error>(())
//! ```
//!
//! [`Value::Bytes`]: https://docs.rs/tessari-types

pub mod envelope;
pub mod error;
pub mod keys;
pub mod seal;
pub mod secret;

pub use envelope::{Binding, Level};
pub use error::{Error, Result};
pub use keys::{Root, Wrapped};
pub use seal::Keyring;
pub use secret::{KEY_BYTES, KeyId, SecretBytes};
