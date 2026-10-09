//! Decision D-16① (`limit_role.matching_key` is SEALED) — the column, the migration, and the
//! boundary where it must fail closed.
//!
//! ## What this file is for
//!
//! `matching_key` is the one column in this database that has held a CLIENT credential in plaintext
//! since the first release. The user's ruling (2026-10-08) was "seal everything, and re-seal legacy
//! rows without a manual step", so there are four things to hold to account, and each has its own
//! test below:
//!
//! 1. a row written NOW is not the key in the clear — asserted against the BARE COLUMN, not through
//!    the API that is supposed to hide it;
//! 2. a row written BEFORE the change is still readable, and the loader re-seals it (that is the
//!    migration: no operator step);
//! 3. a replica rebuilding from the config tree stores it sealed too — the replica is a different
//!    write path (`db/restore.rs`) and would otherwise be the one node still storing the key;
//! 4. the one place this deliberately does NOT fail closed (a plaintext value) is distinguishable
//!    from the place it does (an envelope that will not open) — the second must be an error, never
//!    the ciphertext wearing the key's clothes.
//!
//! Matching itself is asserted here once, through the real loader and the real pure matcher, because
//! "sealed at rest" would be worthless if the hot path stopped seeing the key.

mod common;

use hydra_core::limit::{match_roles, MatchCtx};
use hydra_core::model::LimitRole;
use hydra_server::cluster::content::FidelityRows;
use hydra_server::crypto::{KeyProvider, Sealed, StaticKeyProvider, KEY_LEN};
use hydra_server::{db as repo, store::build_config};

const RAW: &str = "sk-live-customer-key-0001";

fn kp() -> StaticKeyProvider {
    StaticKeyProvider::new([1u8; 32], 1)
}

fn role(id: &str, matching_key: Option<&str>) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: id.into(),
        matching_key: matching_key.map(str::to_string),
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(600),
        limit_token: None,
        window: "m".into(),
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
    }
}

/// The BARE `matching_key` column — what someone reading the database file, a backup or a replica
/// would see. Every "it is sealed" claim in this file is made against THIS, because asking the repo
/// layer returns the opened value by design and would prove nothing.
async fn stored_matching_key(pool: &sqlx::SqlitePool, id: &str) -> Option<String> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT matching_key FROM limit_role WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .expect("select the bare column");
    row.and_then(|r| r.0)
}

/// Write a row the way the code did BEFORE decision D-16: plaintext, straight into the column.
async fn insert_legacy_plaintext_row(pool: &sqlx::SqlitePool, id: &str, key: &str) {
    sqlx::query(
        "INSERT INTO limit_role (id, name, matching_key, limit_count, window, enabled, created_at) \
         VALUES (?, ?, ?, ?, 'm', 1, '2026-01-01 00:00:00')",
    )
    .bind(id)
    .bind(id)
    .bind(key)
    .bind(600_i64)
    .execute(pool)
    .await
    .expect("insert a pre-D-16 plaintext row");
}

/// (1) A new row is sealed, the key still comes back, and it still MATCHES.
///
/// Falsification: bind `r.matching_key` straight into `insert_limit_role` (the pre-D-16 code) and the
/// first assertion fails; drop the `open_limit_key` call from the reader and the last three fail.
#[tokio::test]
async fn a_new_limit_role_is_sealed_at_rest_and_still_matches() {
    let pool = common::setup_pool().await;
    let kp = kp();
    repo::insert_limit_role(&pool, &kp, &role("r-seal", Some(RAW)))
        .await
        .expect("insert");

    let stored = stored_matching_key(&pool, "r-seal")
        .await
        .expect("the row has a matching_key");
    assert_ne!(
        stored, RAW,
        "the bare column must not hold the client key after D-16"
    );
    assert!(
        !stored.contains("sk-live-customer-key"),
        "not even part of the key may survive in the column: {stored}"
    );
    assert!(
        stored.starts_with("sealed:v1:"),
        "the column must hold the envelope form this build writes: {stored}"
    );
    // The envelope is not just opaque text: the master key opens it back to the key.
    let sealed = Sealed::from_text(&stored).expect("an envelope");
    assert_eq!(
        kp.open(&sealed).expect("open"),
        RAW.as_bytes(),
        "the envelope must carry the key it replaced"
    );

    // ...the repo layer returns the KEY (that is what matching needs)...
    let got = repo::get_limit_role(&pool, &kp, "r-seal")
        .await
        .expect("get");
    assert_eq!(got.matching_key.as_deref(), Some(RAW));

    // ...and the hot path really matches on it, through the REAL loader.
    let cfg = build_config(&pool, &kp).await.expect("build_config");
    assert_eq!(cfg.limit_roles.len(), 1, "fixture: one enabled role");
    assert_eq!(cfg.limit_roles[0].matching_key.as_deref(), Some(RAW));
    let matched = match_roles(
        &cfg.limit_roles,
        &MatchCtx {
            api_key: None,
            api_key_raw: Some(RAW),
            model: None,
            tenant: None,
            provider: None,
        },
    );
    assert_eq!(
        matched.len(),
        1,
        "a sealed-at-rest role must still fire for the key it names"
    );
}

/// (1b) An UPDATE re-seals too, and the value changes (a fresh nonce), so this is not a read-path
/// trick that leaves the write path alone.
#[tokio::test]
async fn an_updated_limit_role_is_sealed_again() {
    let pool = common::setup_pool().await;
    let kp = kp();
    repo::insert_limit_role(&pool, &kp, &role("r-up", Some(RAW)))
        .await
        .expect("insert");
    let first = stored_matching_key(&pool, "r-up").await.expect("sealed");

    repo::update_limit_role(&pool, &kp, &role("r-up", Some("sk-second-customer-key")))
        .await
        .expect("update");
    let second = stored_matching_key(&pool, "r-up").await.expect("sealed");

    assert_ne!(
        second, "sk-second-customer-key",
        "UPDATE must seal as well as INSERT"
    );
    assert!(
        second.starts_with("sealed:v1:"),
        "the updated value must be an envelope: {second}"
    );
    assert_ne!(first, second, "a different key must replace the value");
    assert_eq!(
        repo::get_limit_role(&pool, &kp, "r-up")
            .await
            .expect("get")
            .matching_key
            .as_deref(),
        Some("sk-second-customer-key")
    );
}

/// (2) The migration: a row written before D-16 is READABLE, and the loader re-seals it.
///
/// This is the half that makes "no manual step" true, and it is also the half with a wrong-but-easy
/// alternative — inventing an error for a plaintext value.
///
/// Read the assertions in order, because the ORDER is the measured part: the reader must cope with a
/// pre-D-16 row BEFORE any loader has run (an admin `GET /limit-roles` on a node that has not loaded
/// yet), and only then does `build_config` migrate it. An earlier version of this test asserted only
/// the loader half and stayed GREEN when the plaintext pass-through was replaced by an error — the
/// loader seals the row before it reads it, so the reader never saw the plaintext at all. The
/// falsification probe is what exposed that (`.acceptance/round210-probe.out`, P7).
///
/// Falsification: remove the `build_config` call's `seal_legacy_limit_keys` and the bare column stays
/// plaintext (the assertions after the load fail); make `Sealed::open_text` error on a non-envelope
/// and the FIRST one fails.
#[tokio::test]
async fn a_legacy_plaintext_row_is_readable_and_the_loader_reseals_it() {
    let pool = common::setup_pool().await;
    let kp = kp();
    insert_legacy_plaintext_row(&pool, "r-legacy", RAW).await;
    assert_eq!(
        stored_matching_key(&pool, "r-legacy").await.as_deref(),
        Some(RAW),
        "fixture: this row must really start as plaintext"
    );

    // The READER must not refuse a pre-D-16 row, and this is asserted BEFORE the loader ever runs:
    // "not an envelope" means "written before D-16", not "corrupt", and any read that happens first
    // (an admin `GET /limit-roles` on a node whose config load has not reached this row) must still
    // work. Measured while falsifying this file: with the loader's migration in place, the row is
    // already sealed by the time the loader reads it, so an integration test that only went through
    // `build_config` stayed GREEN when the plaintext pass-through was made an error.
    assert_eq!(
        repo::get_limit_role(&pool, &kp, "r-legacy")
            .await
            .expect("a pre-D-16 row must be readable before the migration runs")
            .matching_key
            .as_deref(),
        Some(RAW)
    );

    // The LOADER is what an upgrade runs (boot + every reload), and it migrates.
    let cfg = build_config(&pool, &kp).await.expect("build_config");
    assert_eq!(
        cfg.limit_roles[0].matching_key.as_deref(),
        Some(RAW),
        "a pre-D-16 row must still be READ: refusing it would make an upgrade unable to \
         load its own config"
    );

    let after = stored_matching_key(&pool, "r-legacy")
        .await
        .expect("still has a key");
    assert!(
        !after.contains(RAW),
        "the loader must have rewritten the plaintext row: {after}"
    );
    assert!(
        after.starts_with("sealed:v1:"),
        "…into the envelope form: {after}"
    );
    assert_eq!(
        repo::get_limit_role(&pool, &kp, "r-legacy")
            .await
            .expect("get")
            .matching_key
            .as_deref(),
        Some(RAW),
        "and the key must survive the migration"
    );

    // Idempotent: a second pass has nothing to do. (This is what keeps "the loader ran" from meaning
    // "the loader wrote" on every single reload.)
    assert_eq!(
        repo::seal_legacy_limit_keys(&pool, &kp)
            .await
            .expect("second pass"),
        0,
        "an already-sealed database must need no write at all"
    );
}

// (2b) Where the migration's RACE is tested, and why not here.
//
// The re-seal is a compare-and-swap, and the property that matters lives in the gap between its read
// and its write: a row that changed in between must not be clobbered. An integration test cannot
// reach into that gap, and the first version of this file "tested" it by writing the CAS statement
// out by hand — which stayed green when the production clause was deleted, because it was asserting
// a copy of the code rather than the code. It now lives next to the two production halves it
// composes: `db.rs`, `#[cfg(test)] mod tests::the_legacy_reseal_refuses_a_row_that_changed_after_the_read`.

/// (3) A replica rebuilding from the config tree stores the key sealed as well.
///
/// The replica writes through a DIFFERENT function (`db::restore_config`, which binds the fidelity
/// rows it was handed). It is the path a failover exercises, and if it bound `matching_key` verbatim
/// the fleet would end up with one node — the one that just took over — storing client keys in the
/// clear.
///
/// Falsification: bind `&r.matching_key` in `db/restore.rs` and the first assertion fails.
#[tokio::test]
async fn a_replica_rebuild_stores_the_matching_key_sealed() {
    let leader = common::setup_pool().await;
    let replica = common::setup_pool().await;
    let kp = kp();
    repo::insert_limit_role(&leader, &kp, &role("r-rep", Some(RAW)))
        .await
        .expect("insert");

    let cfg = build_config(&leader, &kp).await.expect("build_config");
    let fidelity = FidelityRows {
        // The fidelity rows carry the key in memory (the matcher needs it); the sealing happens at
        // the boundary that WRITES it, exactly like the provider api-keys.
        limit_roles: cfg.limit_roles.clone(),
        ..FidelityRows::default()
    };
    repo::restore_config(&replica, &kp, &cfg, &fidelity, 1)
        .await
        .expect("restore");

    let stored = stored_matching_key(&replica, "r-rep")
        .await
        .expect("the restored row has a key");
    assert!(
        stored.starts_with("sealed:v1:") && !stored.contains(RAW),
        "a materializing node must not store the client key in the clear: {stored}"
    );
    assert_eq!(
        repo::get_limit_role(&replica, &kp, "r-rep")
            .await
            .expect("get")
            .matching_key
            .as_deref(),
        Some(RAW),
        "…and the key must still be usable on the node that rebuilds"
    );
}

/// (4) The boundary, in both directions: a plaintext value is READ (that is the migration), an
/// envelope that will not open is an ERROR — never the ciphertext, and never silently "no key".
///
/// The second half is the one that matters operationally: a rotation that forgot to keep the old key
/// in the ring must be a loud failure at load, not a node that serves with a garbage `matching_key`
/// (which matches nothing) or, worse, treats it as `None` (which matches EVERY key).
///
/// Falsification: make `open_text` fall back to the raw string when `kp.open` fails and the error
/// assertions fail; make it error on a non-envelope and the first fixture assertion (the legacy row)
/// fails.
#[tokio::test]
async fn an_envelope_that_will_not_open_is_refused_and_never_becomes_a_key() {
    let pool = common::setup_pool().await;
    let kp = kp();
    let other = StaticKeyProvider::new([9u8; KEY_LEN], 1);
    // A row sealed under master-key material this process does not have — a rotation gone wrong.
    let foreign = other.seal(RAW.as_bytes()).expect("seal").to_text();
    sqlx::query(
        "INSERT INTO limit_role (id, name, matching_key, limit_count, window, enabled, created_at) \
         VALUES ('r-foreign', 'r-foreign', ?, 600, 'm', 1, '2026-01-01 00:00:00')",
    )
    .bind(&foreign)
    .execute(&pool)
    .await
    .expect("insert");

    assert!(
        repo::get_limit_role(&pool, &kp, "r-foreign").await.is_err(),
        "an envelope the master key cannot open must be REFUSED, not returned as a key"
    );
    assert!(
        build_config(&pool, &kp).await.is_err(),
        "and the loader must fail loudly rather than serve a config whose limit silently \
         cannot match (a `None` there would match EVERY key)"
    );

    // The same row under the key that CAN open it reads fine — so the refusal above is about the
    // material, not about the format.
    assert_eq!(
        repo::get_limit_role(&pool, &other, "r-foreign")
            .await
            .expect("get with the right key")
            .matching_key
            .as_deref(),
        Some(RAW)
    );
}

/// (4b) A NULL `matching_key` stays NULL (match-all), and it is not turned into an envelope of the
/// empty string on the way through the write path.
///
/// `None` means "this role matches any key" (design §10.1). `Some("")` would mean "matches the key
/// that is the empty string" — a role nobody can ever hit, which is the opposite of the intent.
#[tokio::test]
async fn a_null_matching_key_stays_null_through_every_write_path() {
    let pool = common::setup_pool().await;
    let kp = kp();
    repo::insert_limit_role(&pool, &kp, &role("r-null", None))
        .await
        .expect("insert");
    assert_eq!(
        stored_matching_key(&pool, "r-null").await,
        None,
        "NULL must stay NULL: `Some(\"\")` would match nothing instead of everything"
    );

    let replica = common::setup_pool().await;
    let cfg = build_config(&pool, &kp).await.expect("build_config");
    let fidelity = FidelityRows {
        limit_roles: cfg.limit_roles.clone(),
        ..FidelityRows::default()
    };
    repo::restore_config(&replica, &kp, &cfg, &fidelity, 1)
        .await
        .expect("restore");
    assert_eq!(
        stored_matching_key(&replica, "r-null").await,
        None,
        "the restore path must not seal an absent key into an envelope"
    );
}
