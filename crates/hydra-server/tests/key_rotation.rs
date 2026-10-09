//! Master-key rotation (`db::reseal_secrets`).
//!
//! The hole this closes: `key_version` was hard-coded to 1 at the only
//! construction site, so the versioned-AAD machinery was dead code and
//! "rotate the master key" had no path at all — every stored ciphertext became
//! unopenable, `ConfigStore::load` failed, the process refused to start, and the
//! admin API that could have re-entered the keys runs in that same process.
//!
//! Real SQLite, real AES-GCM, no mocks: the assertions are about the bytes that
//! are actually stored.

use std::sync::Arc;

use hydra_server::crypto::{KeyProvider, StaticKeyProvider, KEY_LEN};
use hydra_server::db;

mod common;

/// The two keys of a rotation: A (old, v1) → B (new, v2).
fn key_a() -> StaticKeyProvider {
    StaticKeyProvider::new([1u8; KEY_LEN], 1)
}
fn key_b() -> StaticKeyProvider {
    StaticKeyProvider::new([2u8; KEY_LEN], 2)
}
/// The rotation window: B seals, both open.
fn ring_a_to_b() -> StaticKeyProvider {
    StaticKeyProvider::with_previous([2u8; KEY_LEN], 2, [1u8; KEY_LEN], 1)
}

async fn seed_provider_key(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider, id: &str, secret: &str) {
    sqlx::query(
        "INSERT INTO provider (id, key, name, endpoint, weight, created_at, updated_at) \
         VALUES ('p1', 'openai', 'O', 'https://api.openai.com', 1, '', '')",
    )
    .execute(pool)
    .await
    .ok();
    let sealed = kp.seal(secret.as_bytes()).expect("seal");
    sqlx::query(
        "INSERT INTO provider_key (id, provider_id, api_key_ciphertext, api_key_nonce, \
         key_version, created_at) VALUES (?, 'p1', ?, ?, ?, '')",
    )
    .bind(id)
    .bind(&sealed.ciphertext)
    .bind(sealed.nonce.to_vec())
    .bind(i64::from(sealed.key_version))
    .execute(pool)
    .await
    .expect("insert provider_key");
}

async fn seed_tenant_cert(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider, id: &str, pem: &str) {
    sqlx::query(
        "INSERT INTO tenant (id, name, domain, auth_url, enabled, created_at, updated_at) \
         VALUES (?, 't', 'localhost', 'http://a/auth', 1, '', '')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("insert tenant");
    let sealed = kp.seal(pem.as_bytes()).expect("seal");
    sqlx::query(
        "UPDATE tenant SET cert_pem = 'CERT PEM', cert_key_ciphertext = ?, cert_key_nonce = ?, \
         cert_key_version = ? WHERE id = ?",
    )
    .bind(&sealed.ciphertext)
    .bind(sealed.nonce.to_vec())
    .bind(i64::from(sealed.key_version))
    .bind(id)
    .execute(pool)
    .await
    .expect("seal tenant cert");
}

/// Seed a `limit_role` whose `matching_key` is sealed under `kp` — the third sealed column, added by
/// decision D-16 (2026-10-08), and the one with no `key_version` column of its own: the version
/// lives INSIDE the envelope, which is why the rotation has to parse it rather than read it.
async fn seed_limit_role_key(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider, id: &str, key: &str) {
    let sealed = kp.seal(key.as_bytes()).expect("seal").to_text();
    sqlx::query(
        "INSERT INTO limit_role (id, name, matching_key, limit_count, window, enabled, created_at) \
         VALUES (?, ?, ?, 600, 'm', 1, '')",
    )
    .bind(id)
    .bind(id)
    .bind(sealed)
    .execute(pool)
    .await
    .expect("insert limit_role");
}

/// Read a stored `matching_key` back through the repo layer (the path the loader uses).
async fn read_limit_key(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider, id: &str) -> Option<String> {
    db::get_limit_role(pool, kp, id)
        .await
        .ok()
        .and_then(|r| r.matching_key)
}

/// Read a stored provider key back through a provider (the same path the loader
/// uses at boot).
async fn read_provider_key(
    pool: &sqlx::SqlitePool,
    kp: &dyn KeyProvider,
    id: &str,
) -> Option<String> {
    let row: Option<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT api_key_ciphertext, api_key_nonce, key_version FROM provider_key WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("select");
    let (ct, nonce, version) = row?;
    let sealed = hydra_server::crypto::Sealed {
        ciphertext: ct,
        nonce: nonce.as_slice().try_into().expect("nonce len"),
        key_version: version as u32,
    };
    kp.open(&sealed)
        .ok()
        .map(|p| String::from_utf8_lossy(&p).into_owned())
}

async fn read_tenant_cert_key(
    pool: &sqlx::SqlitePool,
    kp: &dyn KeyProvider,
    id: &str,
) -> Option<String> {
    let row: Option<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT cert_key_ciphertext, cert_key_nonce, cert_key_version FROM tenant WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("select");
    let (ct, nonce, version) = row?;
    let sealed = hydra_server::crypto::Sealed {
        ciphertext: ct,
        nonce: nonce.as_slice().try_into().expect("nonce len"),
        key_version: version as u32,
    };
    kp.open(&sealed)
        .ok()
        .map(|p| String::from_utf8_lossy(&p).into_owned())
}

/// A row LABELLED with the current version must still be VERIFIED against the
/// current key before it is counted as "already current".
///
/// The version number and the key MATERIAL are independent: rotate the key and
/// forget to bump `HYDRA_ENCRYPTION_KEY_VERSION`, and every row keeps the old
/// number while the ring's slot for that number now holds different bytes. The
/// old code counted "version == current" as proof and exited 0 — the operator's
/// next step is the SOP's "delete the previous key", after which nothing can open
/// the data and the process refuses to start with no way back. Reporting it is the
/// whole point: this test is the difference between a loud failure and a silent
/// one-way door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_labelled_current_but_sealed_with_other_material_is_reported() {
    let pool = common::setup_pool().await;
    // Sealed by material A at version 1 …
    let a = key_a();
    seed_provider_key(&pool, &a, "k1", "sk-old-1").await;
    // … and the operator swaps in material B WITHOUT bumping the version.
    let swapped = StaticKeyProvider::new([3u8; KEY_LEN], 1);

    let report = db::reseal_secrets(&pool, &swapped).await.expect("reseal");

    assert_eq!(
        report.already_current, 0,
        "a row sealed under OTHER material is not 'already current': {report:?}"
    );
    assert!(
        report.failed.iter().any(|m| m.contains("k1")),
        "the row must be NAMED so an operator can act on it: {:?}",
        report.failed
    );
    assert!(
        !report.is_complete(),
        "`is_complete()` drives the exit code, so this must be false: {report:?}"
    );
    // Nothing is silently rewritten under a key that cannot open it.
    assert!(
        read_provider_key(&pool, &a, "k1").await.is_some(),
        "A still opens it"
    );
}

/// The rotation itself: with the new key as current and the old one in the ring,
/// every row is rewritten under the new version — after which the OLD key alone
/// can no longer read anything (the rotation really moved the data).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_reseals_every_secret_under_the_new_version() {
    let pool = common::setup_pool().await;
    let a = key_a();
    seed_provider_key(&pool, &a, "k1", "sk-old-1").await;
    seed_provider_key(&pool, &a, "k2", "sk-old-2").await;
    seed_tenant_cert(&pool, &a, "t1", "-----BEGIN PRIVATE KEY-----old").await;
    seed_limit_role_key(&pool, &a, "r1", "sk-limit-old").await;

    // Before: the new key alone cannot read anything (this is the state that
    // used to be fatal at boot).
    assert!(read_provider_key(&pool, &key_b(), "k1").await.is_none());

    let ring = ring_a_to_b();
    let report = db::reseal_secrets(&pool, &ring).await.expect("reseal");
    assert_eq!(report.provider_keys_resealed, 2, "both provider keys");
    assert_eq!(report.tenant_certs_resealed, 1, "the tenant cert key");
    // Decision D-16: the `limit_role.matching_key` column is sealed too, so a rotation that skipped
    // it would leave every limit role unreadable the moment the previous key is dropped.
    assert_eq!(
        report.limit_keys_resealed, 1,
        "the sealed limit-role key: {report:?}"
    );
    assert!(report.is_complete(), "no failures: {report:?}");

    // After: the NEW key alone opens everything (the ring is no longer needed),
    // and the OLD key alone opens nothing.
    let b = key_b();
    assert_eq!(
        read_provider_key(&pool, &b, "k1").await.as_deref(),
        Some("sk-old-1")
    );
    assert_eq!(
        read_provider_key(&pool, &b, "k2").await.as_deref(),
        Some("sk-old-2")
    );
    assert_eq!(
        read_tenant_cert_key(&pool, &b, "t1").await.as_deref(),
        Some("-----BEGIN PRIVATE KEY-----old")
    );
    assert_eq!(
        read_limit_key(&pool, &b, "r1").await.as_deref(),
        Some("sk-limit-old"),
        "the limit-role key must be readable under the NEW key alone after the rotation"
    );
    assert!(
        read_provider_key(&pool, &a, "k1").await.is_none(),
        "the old key must NOT be able to open a re-sealed row"
    );
    assert!(
        read_limit_key(&pool, &a, "r1").await.is_none(),
        "…nor the limit-role column (it was re-sealed under the new version, or the rotation \
         silently skipped it and this asserts nothing)"
    );
    // The public cert PEM is untouched (it is public material).
    let cert_pem: (String,) = sqlx::query_as("SELECT cert_pem FROM tenant WHERE id = 't1'")
        .fetch_one(&pool)
        .await
        .expect("cert pem");
    assert_eq!(cert_pem.0, "CERT PEM");

    // Idempotent: a second pass finds everything already current.
    let again = db::reseal_secrets(&pool, &ring)
        .await
        .expect("reseal twice");
    assert_eq!(again.provider_keys_resealed, 0);
    assert_eq!(again.tenant_certs_resealed, 0);
    assert_eq!(again.limit_keys_resealed, 0);
    assert_eq!(
        again.already_current, 4,
        "2 provider keys + 1 tenant cert + 1 limit-role key"
    );
    assert!(again.is_complete());
}

/// A row that is still PLAINTEXT is sealed by the rotation pass too (decision D-16).
///
/// The loader is the usual migrator, but `hydra --reseal` runs BEFORE it (and can be run on its own),
/// so it must not walk past a pre-D-16 row and leave the operator's rotation "complete" while a
/// client key is still sitting in the clear. Either pass may be the one that gets there first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rotation_also_seals_a_plaintext_limit_key_it_finds() {
    let pool = common::setup_pool().await;
    // The way the pre-D-16 code wrote it: plaintext, straight into the column.
    sqlx::query(
        "INSERT INTO limit_role (id, name, matching_key, limit_count, window, enabled, created_at) \
         VALUES ('r-legacy', 'r-legacy', 'sk-legacy-plaintext', 600, 'm', 1, '')",
    )
    .execute(&pool)
    .await
    .expect("insert");

    let report = db::reseal_secrets(&pool, &key_a()).await.expect("reseal");
    assert_eq!(
        report.limit_keys_resealed, 1,
        "a plaintext row must be sealed by this pass: {report:?}"
    );
    assert!(report.is_complete(), "{report:?}");

    let stored: (Option<String>,) =
        sqlx::query_as("SELECT matching_key FROM limit_role WHERE id = 'r-legacy'")
            .fetch_one(&pool)
            .await
            .expect("select");
    assert!(
        stored
            .0
            .as_deref()
            .is_some_and(|v| v.starts_with("sealed:v1:")),
        "the column must hold an envelope afterwards: {:?}",
        stored.0
    );
    assert_eq!(
        read_limit_key(&pool, &key_a(), "r-legacy").await.as_deref(),
        Some("sk-legacy-plaintext"),
        "…and the key itself must still come back"
    );
}

/// A row nobody can open is REPORTED and left alone — never rewritten with
/// garbage, and never silently counted as rotated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unopenable_row_is_reported_and_left_untouched() {
    let pool = common::setup_pool().await;
    let stranger = StaticKeyProvider::new([9u8; KEY_LEN], 7);
    seed_provider_key(&pool, &stranger, "k9", "sk-unknown-key").await;
    let a = key_a();
    seed_provider_key(&pool, &a, "k1", "sk-old-1").await;

    let ring = ring_a_to_b();
    let report = db::reseal_secrets(&pool, &ring).await.expect("reseal");
    assert_eq!(
        report.provider_keys_resealed, 1,
        "only the readable row moved"
    );
    assert_eq!(report.failed.len(), 1, "the unknown version is reported");
    assert!(
        report.failed[0].contains("k9") && report.failed[0].contains('7'),
        "the report must name the row and the version: {:?}",
        report.failed
    );
    assert!(!report.is_complete(), "the caller must exit non-zero");

    // Left untouched: still sealed under version 7, and still unreadable by the
    // rotation's ring.
    let version: (i64,) = sqlx::query_as("SELECT key_version FROM provider_key WHERE id = 'k9'")
        .fetch_one(&pool)
        .await
        .expect("version");
    assert_eq!(version.0, 7, "the unreadable row was not rewritten");
    assert!(read_provider_key(&pool, &ring, "k9").await.is_none());
}

/// The ring is what makes the version machinery real: a versioned provider
/// refuses a version it does not hold (fail-closed), and the ring reports it.
#[test]
fn the_key_ring_refuses_versions_it_does_not_hold() {
    let ring = ring_a_to_b();
    assert_eq!(ring.version(), 2, "the current version seals");
    assert!(ring.has_version(1) && ring.has_version(2));
    assert!(!ring.has_version(3));

    let sealed = ring.seal(b"secret").expect("seal");
    assert_eq!(
        sealed.key_version, 2,
        "new ciphertext carries the NEW version"
    );

    // A provider holding only version 1 cannot open version-2 ciphertext.
    let only_a = key_a();
    assert!(only_a.open(&sealed).is_err());
    // …and the trait is where the version lives, so a re-seal target is reachable
    // through `&dyn KeyProvider`.
    let dynamic: Arc<dyn KeyProvider> = Arc::new(ring);
    assert_eq!(dynamic.version(), 2);
}
