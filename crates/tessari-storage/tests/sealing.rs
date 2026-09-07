//! What a vault write actually puts in the store.
//!
//! Every test here asserts against **bytes**, not against the sealing code's own
//! account of itself. That is the whole method: reading the encryption path and
//! concluding it encrypts is the check that has never once caught this class of
//! bug, because the code always looks right — the failure is a path that never
//! called it.
//!
//! The scan for a planted plaintext is the beginning of criterion K2, not the
//! whole of it. K2 also wants the same scan against a real backup artifact and
//! against a follower, which are separate code paths and are separate tests.
//! Nothing here may be cited as K2 passing.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{
    Catalog, Error, FieldShape, KEYS_FIELD, RecordAddress, Store, TableDefinition, TableKind,
    TableShape, VAULT_RECIPIENT, VaultDeclaration, open_data_key, open_field, seal_secrets,
    vault_key_scope,
};
use tessari_types::{FieldKind, RecordId, Value};
use tessari_vault::{Level, Root, keys};

/// A distinctive string. If this turns up in the stored bytes, sealing did not
/// happen — and a short or common word would turn up by coincidence.
const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const PASSPHRASE: &str = "an operator's passphrase, presented by statement";

struct Fixture {
    backend: Arc<dyn KvBackend>,
    store: Store,
    vault: TableDefinition,
}

/// A store with its keyring unsealed and one vault holding one secret field.
///
/// The vault is built through the catalog rather than through `DEFINE VAULT`,
/// which does not exist yet. That is a real limitation of this test and it is
/// named rather than hidden: it exercises the write path and the key hierarchy,
/// and it does not exercise the statement that will one day set them up.
fn fixture(secret: bool) -> Fixture {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();

    let (root, master) = Root::create(PASSPHRASE).unwrap();
    store.vault().adopt(master).unwrap();

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "people").unwrap();

    // The vault's own key, wrapped under the master key, bound to the name the
    // table is about to claim — the same binding the write path recomputes.
    let scope = vault_key_scope(namespace.id, database.id, "credentials");
    let wrapped = store
        .vault()
        .with_master(|master| Ok(keys::wrap_fresh(master, Level::Vault, &scope)?.0))
        .unwrap();

    let vault = catalog
        .create_table(
            namespace.id,
            database.id,
            "credentials",
            TableShape {
                kind: TableKind::Vault(VaultDeclaration { key: wrapped }),
                ..TableShape::default()
            },
        )
        .unwrap();
    catalog
        .create_field(
            vault.id,
            "password",
            FieldKind::Any,
            FieldShape {
                secret,
                ..FieldShape::default()
            },
        )
        .unwrap();
    transaction.commit().unwrap();

    // Kept so a test can prove the root record is safe to hold beside the data.
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction).set_vault_root(&tessari_storage::VaultRoot(root));
    transaction.commit().unwrap();

    Fixture {
        backend,
        store,
        vault,
    }
}

impl Fixture {
    fn address(&self) -> RecordAddress {
        RecordAddress::new(
            self.vault.namespace,
            self.vault.database,
            self.vault.id,
            RecordId::Text("ada".to_owned()),
        )
    }

    /// A record carrying the planted secret and one ordinary field beside it.
    fn record() -> Value {
        Value::Object(
            [
                ("name".to_owned(), Value::String("Ada".to_owned())),
                ("password".to_owned(), Value::String(PLANTED.to_owned())),
            ]
            .into_iter()
            .collect(),
        )
    }

    /// Seal a record and write it, exactly as the session's write path does.
    fn write(&self) -> Value {
        let mut transaction = self.store.begin().unwrap();
        let address = self.address();
        let sealed = seal_secrets(&mut transaction, &address, Self::record()).unwrap();
        transaction.put(address, encode_payload(&sealed).into_bytes());
        transaction.commit().unwrap();
        sealed
    }

    /// Every byte the backend holds, concatenated.
    ///
    /// The point of scanning the **backend** rather than the record is that the
    /// record is what the sealing path returns and the backend is what an
    /// attacker copies. They are the same bytes only if nothing between them
    /// wrote a second copy.
    fn all_stored_bytes(&self) -> Vec<u8> {
        let mut everything = Vec::new();
        for keyspace in tessari_kv::Keyspace::ALL {
            let scan = tessari_kv::ScanRequest {
                keyspace: *keyspace,
                range: tessari_kv::KeyRange::all(),
                direction: tessari_kv::ScanDirection::Forward,
                limit: None,
            };
            for entry in self.backend.scan(&scan).unwrap() {
                everything.extend_from_slice(entry.0.as_slice());
                everything.extend_from_slice(entry.1.as_slice());
            }
        }
        everything
    }
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn the_planted_secret_is_nowhere_in_the_backend() {
    let fixture = fixture(true);
    fixture.write();

    let stored = fixture.all_stored_bytes();
    assert!(
        !holds(&stored, PLANTED),
        "the planted secret is stored in the clear"
    );
    // The control. Without it a bug that stored nothing at all — a write that
    // silently did not happen — would pass the assertion above, and the test
    // would report the strongest possible result for the weakest possible
    // reason.
    assert!(
        holds(&stored, "Ada"),
        "the record was not written at all, so the scan above proves nothing"
    );
}

#[test]
fn a_field_that_is_not_secret_is_stored_as_it_was_written() {
    let fixture = fixture(false);
    fixture.write();

    let stored = fixture.all_stored_bytes();
    assert!(
        holds(&stored, PLANTED),
        "an undeclared-secret field was sealed anyway: sealing is deciding for \
         itself which fields are secret"
    );
}

#[test]
fn a_sealed_field_opens_back_to_the_value_that_was_written() {
    let fixture = fixture(true);
    let sealed = fixture.write();
    let Value::Object(fields) = &sealed else {
        panic!("a vault record is an object")
    };

    let transaction = fixture.store.begin().unwrap();
    let address = fixture.address();
    let data_key = open_data_key(&transaction, &address, &fixture.vault, fields).unwrap();
    let Some(Value::Bytes(envelope)) = fields.get("password") else {
        panic!("the secret field holds an envelope")
    };
    let opened = open_field(&data_key, &address, &fixture.vault, "password", envelope).unwrap();
    transaction.rollback();

    assert_eq!(opened, Value::String(PLANTED.to_owned()));
}

#[test]
fn a_ciphertext_moved_to_another_record_does_not_open() {
    let fixture = fixture(true);
    let sealed = fixture.write();
    let Value::Object(fields) = &sealed else {
        panic!("a vault record is an object")
    };
    let Some(Value::Bytes(envelope)) = fields.get("password") else {
        panic!("the secret field holds an envelope")
    };

    let transaction = fixture.store.begin().unwrap();
    let address = fixture.address();
    let data_key = open_data_key(&transaction, &address, &fixture.vault, fields).unwrap();

    // The same key, the same field, a different record. This is what an attacker
    // with write access to the store does: not break the cipher, but move a
    // ciphertext to a row they are allowed to read.
    let elsewhere = RecordAddress::new(
        fixture.vault.namespace,
        fixture.vault.database,
        fixture.vault.id,
        RecordId::Text("grace".to_owned()),
    );
    let moved = open_field(&data_key, &elsewhere, &fixture.vault, "password", envelope);
    assert!(matches!(moved, Err(Error::Vault(_))), "{moved:?}");
    transaction.rollback();
}

#[test]
fn a_ciphertext_moved_to_another_field_does_not_open() {
    let fixture = fixture(true);
    let sealed = fixture.write();
    let Value::Object(fields) = &sealed else {
        panic!("a vault record is an object")
    };
    let Some(Value::Bytes(envelope)) = fields.get("password") else {
        panic!("the secret field holds an envelope")
    };

    let transaction = fixture.store.begin().unwrap();
    let address = fixture.address();
    let data_key = open_data_key(&transaction, &address, &fixture.vault, fields).unwrap();
    let moved = open_field(&data_key, &address, &fixture.vault, "recovery", envelope);
    assert!(matches!(moved, Err(Error::Vault(_))), "{moved:?}");
    transaction.rollback();
}

#[test]
fn a_sealed_store_refuses_the_write_rather_than_writing_plaintext() {
    let fixture = fixture(true);
    fixture.store.vault().seal().unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    let address = fixture.address();
    let refused = seal_secrets(&mut transaction, &address, Fixture::record());
    assert!(
        matches!(refused, Err(Error::Vault(tessari_vault::Error::Sealed))),
        "{refused:?}"
    );
    transaction.rollback();

    // The refusal is only worth anything if nothing was written on the way to
    // it. A sealing path that half-wrote and then failed would leave the
    // plaintext behind and report an error.
    assert!(!holds(&fixture.all_stored_bytes(), PLANTED));
}

#[test]
fn a_write_may_not_supply_the_key_set_itself() {
    let fixture = fixture(true);
    let mut transaction = fixture.store.begin().unwrap();
    let address = fixture.address();

    let forged = Value::Object(
        [
            ("password".to_owned(), Value::String(PLANTED.to_owned())),
            (
                KEYS_FIELD.to_owned(),
                Value::Object(
                    [(VAULT_RECIPIENT.to_owned(), Value::Bytes(vec![0; 64]))]
                        .into_iter()
                        .collect(),
                ),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let refused = seal_secrets(&mut transaction, &address, forged);
    assert!(
        matches!(refused, Err(Error::VaultReservedField { .. })),
        "{refused:?}"
    );
    transaction.rollback();
}

#[test]
fn an_ordinary_table_is_left_alone() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "people").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "staff", TableShape::default())
        .unwrap();
    transaction.commit().unwrap();

    // Sealed keyring, ordinary table. If the vault path ran at all this would
    // refuse — which is the cheap way to assert that it did not run.
    let mut transaction = store.begin().unwrap();
    let address = RecordAddress::new(
        namespace.id,
        database.id,
        table.id,
        RecordId::Text("ada".to_owned()),
    );
    let untouched = seal_secrets(&mut transaction, &address, Fixture::record()).unwrap();
    transaction.rollback();

    assert_eq!(untouched, Fixture::record());
}

#[test]
fn the_stored_record_still_decodes_as_an_ordinary_payload() {
    let fixture = fixture(true);
    let sealed = fixture.write();

    // A build that has never heard of vaults reads this record as a plain table
    // and gets ciphertext, which is the property that makes the boundary worth
    // having. It is also Q-413: the bytes hold, and every refusal the word
    // carries is gone.
    let round_tripped = decode_payload(&encode_payload(&sealed).into_bytes()).unwrap();
    assert_eq!(round_tripped, sealed);

    let Value::Object(fields) = &round_tripped else {
        panic!("a vault record is an object")
    };
    assert!(matches!(fields.get("password"), Some(Value::Bytes(_))));
    assert!(matches!(fields.get(KEYS_FIELD), Some(Value::Object(_))));
    assert_eq!(fields.get("name"), Some(&Value::String("Ada".to_owned())));
}

/// Four vaults, all called `credentials`, in four tenancies of one store.
///
/// Two of them carry a key that was minted for somebody else's qualified name —
/// literally the same wrapped bytes, lifted out of `prod.people`'s declaration
/// and pasted into theirs, which is what an operator with catalog access can do
/// and what a bug in scope construction would do by accident.
struct Crossing {
    store: Store,
    /// `prod.people.credentials`, minted for its own name.
    own: TableDefinition,
    /// `prod.payroll.credentials`, carrying `prod.people`'s key.
    across_databases: TableDefinition,
    /// `staging.people.credentials`, carrying `prod.people`'s key.
    across_namespaces: TableDefinition,
    /// `prod.finance.credentials`, minted for itself. The control.
    control: TableDefinition,
}

fn crossing() -> Crossing {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(backend).unwrap();
    let (_root, master) = Root::create(PASSPHRASE).unwrap();
    store.vault().adopt(master).unwrap();

    // The tenancies first, because the scope is built from their ids and the
    // ids do not exist until the catalog allocates them.
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let prod = catalog.create_namespace("prod").unwrap().id;
    let staging = catalog.create_namespace("staging").unwrap().id;
    let people = catalog.create_database(prod, "people").unwrap().id;
    let payroll = catalog.create_database(prod, "payroll").unwrap().id;
    let finance = catalog.create_database(prod, "finance").unwrap().id;
    let other_people = catalog.create_database(staging, "people").unwrap().id;
    transaction.commit().unwrap();

    let mint = |namespace, database| {
        let scope = vault_key_scope(namespace, database, "credentials");
        store
            .vault()
            .with_master(|master| Ok(keys::wrap_fresh(master, Level::Vault, &scope)?.0))
            .unwrap()
    };
    let prod_people_key = mint(prod, people);
    let finance_key = mint(prod, finance);

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let mut declared = Vec::new();
    for (namespace, database, key) in [
        (prod, people, prod_people_key.clone()),
        (prod, payroll, prod_people_key.clone()),
        (staging, other_people, prod_people_key),
        (prod, finance, finance_key),
    ] {
        let table = catalog
            .create_table(
                namespace,
                database,
                "credentials",
                TableShape {
                    kind: TableKind::Vault(VaultDeclaration { key }),
                    ..TableShape::default()
                },
            )
            .unwrap();
        catalog
            .create_field(
                table.id,
                "password",
                FieldKind::Any,
                FieldShape {
                    secret: true,
                    ..FieldShape::default()
                },
            )
            .unwrap();
        declared.push(table);
    }
    transaction.commit().unwrap();

    let control = declared.pop().unwrap();
    let across_namespaces = declared.pop().unwrap();
    let across_databases = declared.pop().unwrap();
    let own = declared.pop().unwrap();
    Crossing {
        store,
        own,
        across_databases,
        across_namespaces,
        control,
    }
}

/// Seal and write one record into `vault`, returning whatever the store said.
///
/// Not `?`-shaped: a failing seal leaves the transaction open, and every other
/// test in this file ends one explicitly rather than trusting a drop.
fn attempt_write(store: &Store, vault: &TableDefinition) -> Result<(), Error> {
    let mut transaction = store.begin().unwrap();
    let address = RecordAddress::new(
        vault.namespace,
        vault.database,
        vault.id,
        RecordId::Text("ada".to_owned()),
    );
    match seal_secrets(&mut transaction, &address, Fixture::record()) {
        Ok(sealed) => {
            transaction.put(address, encode_payload(&sealed).into_bytes());
            transaction.commit().map(|_| ())
        }
        Err(error) => {
            transaction.rollback();
            Err(error)
        }
    }
}

/// Row 16 of the negative matrix — the tenancy crossing.
///
/// A vault's key is wrapped under the store's master key with the vault's
/// **qualified name** as its associated data, so a key minted for
/// `prod.people.credentials` is not a key for `prod.payroll.credentials` even
/// though both vaults carry the same name, sit in the same store and are
/// wrapped under the same master key. Until this test the property was
/// structural and unobserved — `vault_key_scope` is the one function that
/// computes the binding, so the whole argument for it was a code reading, and a
/// code reading is exactly what this class of bug survives.
///
/// Both axes are crossed, because they would be broken by different mistakes —
/// dropping the database from the binding, and dropping the namespace — and a
/// test of one says nothing about the other. The control writes with a key
/// minted correctly, so a refusal produced by the fixture rather than by the
/// binding cannot pass as this property.
#[test]
fn a_vault_key_from_another_tenancy_does_not_open_this_vault() {
    let crossing = crossing();

    let own = attempt_write(&crossing.store, &crossing.own);
    assert!(own.is_ok(), "{own:?}");
    let control = attempt_write(&crossing.store, &crossing.control);
    assert!(control.is_ok(), "{control:?}");

    let across_databases = attempt_write(&crossing.store, &crossing.across_databases);
    assert!(
        matches!(across_databases, Err(Error::Vault(_))),
        "a key minted for another database in the same namespace opened this one: \
         {across_databases:?}"
    );

    let across_namespaces = attempt_write(&crossing.store, &crossing.across_namespaces);
    assert!(
        matches!(across_namespaces, Err(Error::Vault(_))),
        "a key minted for a database of the same name in another namespace opened \
         this one: {across_namespaces:?}"
    );
}
