//! Shared by the HTTP integration tests.

use aurora_store::Store;
use sha2::{Digest, Sha256};

pub mod harness;

/// A fixed key for tests that are not about authentication.
const TEST_KEY: &str = "aur_integration_tests_only_not_a_secret";

/// Inserts an owner and a write-scope key straight into the tables, so the
/// event log stays empty and tests that count events are unaffected, and
/// returns the key for `Authorization: Bearer`.
pub async fn owner_key(store: &Store) -> String {
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind, role) VALUES (1, 'owner', 'human', 'owner')",
                [],
            )?;
            tx.execute(
                "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
                 VALUES (1, 'tests', 'aur_inte', ?1, 'write', 1)",
                [format!("{:x}", Sha256::digest(TEST_KEY.as_bytes()))],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    TEST_KEY.to_string()
}
