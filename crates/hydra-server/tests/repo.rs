//! §2.2 — repo CRUD per entity (real `:memory:` SQLite, no mock).

mod common;

use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
};
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::db as repo;

fn now() -> &'static str {
    "2026-01-01 00:00:00"
}

/// Deterministic test key provider (never reads from the environment).
fn kp() -> StaticKeyProvider {
    StaticKeyProvider::new([1u8; 32], 1)
}

fn provider(id: &str, key: &str, weight: i32) -> Provider {
    Provider {
        id: id.into(),
        key: key.into(),
        name: format!("{key} name"),
        endpoint: format!("https://{key}.example.com"),
        weight,
        created_at: now().into(),
        updated_at: now().into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn model(id: &str, key: &str, provider_id: &str, status: i32) -> ProviderModel {
    ProviderModel {
        id: id.into(),
        key: key.into(),
        name: format!("{key}-model"),
        provider_id: provider_id.into(),
        status,
    }
}

fn tenant(id: &str, domain: &str, enabled: bool) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("{id}-tenant"),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled,
        created_at: now().into(),
        updated_at: now().into(),
    }
}

/// T4.1 — provider CRUD incl. UNIQUE `key` conflict.
#[tokio::test]
async fn provider_crud() {
    let pool = common::setup_pool().await;

    let p = provider("p1", "openai", 5);
    repo::insert_provider(&pool, &p).await.expect("insert");

    let got = repo::get_provider(&pool, "p1").await.expect("get");
    assert_eq!(got, p);

    let listed = repo::list_providers(&pool).await.expect("list");
    assert_eq!(listed.len(), 1);

    let mut updated = p.clone();
    updated.weight = 9;
    updated.name = "renamed".into();
    repo::update_provider(&pool, &updated)
        .await
        .expect("update");
    let got2 = repo::get_provider(&pool, "p1").await.expect("get2");
    assert_eq!(got2.weight, 9);
    assert_eq!(got2.name, "renamed");

    repo::delete_provider(&pool, "p1").await.expect("delete");
    assert!(
        repo::get_provider(&pool, "p1").await.is_err(),
        "provider should be gone after delete"
    );

    // UNIQUE(key) conflict.
    repo::insert_provider(&pool, &provider("pA", "dup", 1))
        .await
        .expect("first dup insert");
    let err = repo::insert_provider(&pool, &provider("pB", "dup", 1))
        .await
        .expect_err("duplicate key must error");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_unique_violation()),
        "expected UNIQUE violation, got {err:?}"
    );
}

/// T4.2 — provider_model CRUD incl. status CHECK, UNIQUE(key,provider_id),
/// and CASCADE on provider delete.
#[tokio::test]
async fn provider_model_crud() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("provider");

    let m = model("m1", "gpt-4", "p1", 1);
    repo::insert_provider_model(&pool, &m)
        .await
        .expect("insert");
    assert_eq!(repo::get_provider_model(&pool, "m1").await.unwrap(), m);

    // status CHECK (1/0/-1) — anything else must error.
    let bad = model("mbad", "gpt-4", "p1", 7);
    let err = repo::insert_provider_model(&pool, &bad)
        .await
        .expect_err("bad status must violate CHECK");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_check_violation()),
        "expected CHECK violation, got {err:?}"
    );

    // UNIQUE(key, provider_id).
    let dup = model("m2", "gpt-4", "p1", 0);
    let err = repo::insert_provider_model(&pool, &dup)
        .await
        .expect_err("duplicate (key,provider_id) must error");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_unique_violation()),
        "expected UNIQUE violation, got {err:?}"
    );

    // A second provider can host the same model key (different provider_id).
    repo::insert_provider(&pool, &provider("p2", "azure", 1))
        .await
        .expect("provider2");
    repo::insert_provider_model(&pool, &model("m3", "gpt-4", "p2", 1))
        .await
        .expect("same key different provider");

    // update
    let mut off = m.clone();
    off.status = 0;
    repo::update_provider_model(&pool, &off)
        .await
        .expect("update");
    assert_eq!(
        repo::get_provider_model(&pool, "m1").await.unwrap().status,
        0
    );

    // CASCADE: deleting p1 removes its models.
    repo::delete_provider(&pool, "p1").await.expect("delete p1");
    assert!(
        repo::get_provider_model(&pool, "m1").await.is_err(),
        "model must be CASCADE-deleted with its provider"
    );
    // p2's model survives.
    assert!(repo::get_provider_model(&pool, "m3").await.is_ok());
}

/// T4.3 — provider_key CRUD: list-by-provider + CASCADE.
#[tokio::test]
async fn provider_key_crud() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("provider");

    let k1 = ProviderKey {
        id: "k1".into(),
        provider_id: "p1".into(),
        api_key: "sk-aaa".into(),
        created_at: now().into(),
    };
    let k2 = ProviderKey {
        id: "k2".into(),
        provider_id: "p1".into(),
        api_key: "sk-bbb".into(),
        created_at: now().into(),
    };
    repo::insert_provider_key(&pool, &kp(), &k1)
        .await
        .expect("k1");
    repo::insert_provider_key(&pool, &kp(), &k2)
        .await
        .expect("k2");

    let by_p = repo::list_provider_keys_by_provider(&pool, &kp(), "p1")
        .await
        .expect("list by provider");
    assert_eq!(by_p.len(), 2);

    repo::delete_provider_key(&pool, "k1")
        .await
        .expect("del k1");
    let after = repo::list_provider_keys_by_provider(&pool, &kp(), "p1")
        .await
        .expect("list after");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, "k2");

    // CASCADE: deleting provider removes keys.
    repo::delete_provider(&pool, "p1").await.expect("del p1");
    let cascaded = repo::list_provider_keys_by_provider(&pool, &kp(), "p1")
        .await
        .expect("list cascaded");
    assert!(
        cascaded.is_empty(),
        "keys must CASCADE-delete with provider"
    );
}

/// T4.4 — tenant CRUD incl. `auth_url NOT NULL`, `domain UNIQUE`, `enabled` CHECK.
#[tokio::test]
async fn tenant_crud() {
    let pool = common::setup_pool().await;

    let t = tenant("t1", "acme.com", true);
    repo::insert_tenant(&pool, &t).await.expect("insert");

    let got = repo::get_tenant(&pool, "t1").await.expect("get");
    assert_eq!(got, t);
    assert_eq!(got.auth_url, "https://auth.acme.com/verify");

    // domain UNIQUE conflict.
    let err = repo::insert_tenant(&pool, &tenant("t2", "acme.com", true))
        .await
        .expect_err("duplicate domain");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_unique_violation()),
        "expected UNIQUE violation on domain, got {err:?}"
    );

    // enabled CHECK: SQL bypass via raw insert with enabled=2.
    let err = sqlx::query(
        "INSERT INTO tenant (id, name, domain, auth_url, enabled) \
         VALUES ('tbad','bad','bad.com', 'u', 2)",
    )
    .execute(&pool)
    .await
    .expect_err("enabled=2 must violate CHECK");
    let db_err = match err {
        sqlx::Error::Database(d) => d,
        other => panic!("expected database error, got {other:?}"),
    };
    assert!(
        db_err.is_check_violation(),
        "expected CHECK violation on enabled, got {db_err}"
    );

    // update + delete.
    let mut upd = t.clone();
    upd.enabled = false;
    repo::update_tenant(&pool, &upd).await.expect("update");
    assert!(!repo::get_tenant(&pool, "t1").await.unwrap().enabled);

    repo::delete_tenant(&pool, "t1").await.expect("delete");
    assert!(repo::get_tenant(&pool, "t1").await.is_err());
}

/// T4.5 — tenant_provider CRUD: UNIQUE(tenant_id,provider_id) + FK cascade.
#[tokio::test]
async fn tenant_provider_crud() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("provider");
    repo::insert_tenant(&pool, &tenant("t1", "acme.com", true))
        .await
        .expect("tenant");

    let tp = TenantProvider {
        id: "tp1".into(),
        tenant_id: "t1".into(),
        provider_id: "p1".into(),
    };
    repo::insert_tenant_provider(&pool, &tp)
        .await
        .expect("insert");
    assert_eq!(repo::get_tenant_provider(&pool, "tp1").await.unwrap(), tp);

    // UNIQUE(tenant_id, provider_id).
    let err = repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp2".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect_err("duplicate (tenant,provider)");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_unique_violation()),
        "expected UNIQUE violation, got {err:?}"
    );

    // CASCADE: deleting tenant removes the link.
    repo::delete_tenant(&pool, "t1")
        .await
        .expect("delete tenant");
    assert!(
        repo::get_tenant_provider(&pool, "tp1").await.is_err(),
        "tenant_provider must CASCADE-delete with tenant"
    );
}

/// T4.6 — tenant_model CRUD: UNIQUE(tenant_id, model_key).
#[tokio::test]
async fn tenant_model_crud() {
    let pool = common::setup_pool().await;
    repo::insert_tenant(&pool, &tenant("t1", "acme.com", true))
        .await
        .expect("tenant");

    let tm = TenantModel {
        id: "tm1".into(),
        tenant_id: "t1".into(),
        model_key: "gpt-4".into(),
    };
    repo::insert_tenant_model(&pool, &tm).await.expect("insert");
    assert_eq!(repo::get_tenant_model(&pool, "tm1").await.unwrap(), tm);

    let err = repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm2".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect_err("duplicate (tenant,model_key)");
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.is_unique_violation()),
        "expected UNIQUE violation, got {err:?}"
    );

    repo::delete_tenant_model(&pool, "tm1")
        .await
        .expect("delete");
    assert!(repo::get_tenant_model(&pool, "tm1").await.is_err());
}

/// T4.7 — limit_role CRUD: window CHECK (m/h/d) + enabled CHECK.
#[tokio::test]
async fn limit_role_crud() {
    let pool = common::setup_pool().await;

    let role = LimitRole {
        id: "lr1".into(),
        name: "global-1k-min".into(),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(1000),
        limit_token: None,
        window: "m".into(),
        enabled: true,
        created_at: now().into(),
    };
    repo::insert_limit_role(&pool, &role).await.expect("insert");
    let got = repo::get_limit_role(&pool, "lr1").await.expect("get");
    assert_eq!(got, role);

    // window CHECK: invalid value.
    let err = sqlx::query("INSERT INTO limit_role (id, name, window) VALUES ('lrbad','bad','s')")
        .execute(&pool)
        .await
        .expect_err("window='s' must violate CHECK");
    let db_err = match err {
        sqlx::Error::Database(d) => d,
        other => panic!("expected database error, got {other:?}"),
    };
    assert!(
        db_err.is_check_violation(),
        "expected CHECK violation on window, got {db_err}"
    );

    // enabled CHECK: invalid value.
    let err = sqlx::query(
        "INSERT INTO limit_role (id, name, window, enabled) VALUES ('lrbad2','bad','h', 3)",
    )
    .execute(&pool)
    .await
    .expect_err("enabled=3 must violate CHECK");
    let db_err = match err {
        sqlx::Error::Database(d) => d,
        other => panic!("expected database error, got {other:?}"),
    };
    assert!(
        db_err.is_check_violation(),
        "expected CHECK violation on enabled, got {db_err}"
    );

    // update + delete.
    let mut upd = role.clone();
    upd.limit_count = Some(500);
    upd.window = "h".into();
    repo::update_limit_role(&pool, &upd).await.expect("update");
    let got2 = repo::get_limit_role(&pool, "lr1").await.unwrap();
    assert_eq!(got2.limit_count, Some(500));
    assert_eq!(got2.window, "h");

    repo::delete_limit_role(&pool, "lr1").await.expect("delete");
    assert!(repo::get_limit_role(&pool, "lr1").await.is_err());
}

/// T4.8 — complex round-trip: 1 tenant + 2 providers + models + keys + links,
/// verifying the written graph reads back identically.
#[tokio::test]
async fn repo_insert_then_query_roundtrip() {
    let pool = common::setup_pool().await;

    repo::insert_provider(&pool, &provider("p1", "openai", 3))
        .await
        .expect("p1");
    repo::insert_provider(&pool, &provider("p2", "azure", 7))
        .await
        .expect("p2");
    repo::insert_provider_model(&pool, &model("m1", "gpt-4", "p1", 1))
        .await
        .expect("m1");
    repo::insert_provider_model(&pool, &model("m2", "gpt-4", "p2", 1))
        .await
        .expect("m2");
    repo::insert_provider_model(&pool, &model("m3", "dall-e", "p1", 0))
        .await
        .expect("m3 offline");
    repo::insert_provider_key(
        &pool,
        &kp(),
        &ProviderKey {
            id: "k1".into(),
            provider_id: "p1".into(),
            api_key: "sk-openai".into(),
            created_at: now().into(),
        },
    )
    .await
    .expect("k1");
    repo::insert_provider_key(
        &pool,
        &kp(),
        &ProviderKey {
            id: "k2".into(),
            provider_id: "p2".into(),
            api_key: "sk-azure".into(),
            created_at: now().into(),
        },
    )
    .await
    .expect("k2");
    repo::insert_tenant(&pool, &tenant("t1", "acme.com", true))
        .await
        .expect("t1");
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("tp1");
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp2".into(),
            tenant_id: "t1".into(),
            provider_id: "p2".into(),
        },
    )
    .await
    .expect("tp2");
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("tm1");

    // Verify the graph reads back consistently.
    let providers = repo::list_providers(&pool).await.expect("providers");
    assert_eq!(providers.len(), 2);
    let models = repo::list_provider_models(&pool).await.expect("models");
    assert_eq!(models.len(), 3);
    let keys = repo::list_provider_keys(&pool, &kp()).await.expect("keys");
    assert_eq!(keys.len(), 2);
    let tps = repo::list_tenant_providers(&pool).await.expect("tps");
    assert_eq!(tps.len(), 2);
    let tms = repo::list_tenant_models(&pool).await.expect("tms");
    assert_eq!(tms.len(), 1);
    let tenants = repo::list_tenants(&pool).await.expect("tenants");
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].domain, "acme.com");
    assert_eq!(tenants[0].auth_url, "https://auth.acme.com/verify");
}

/// T4.9 — provider keys are encrypted at rest (AES-256-GCM).
///
/// The raw `api_key_ciphertext` BLOB in the DB must NOT equal the plaintext;
/// the decrypted round-trip via `list_provider_keys` must recover it; and a
/// different master key must fail to decrypt.
#[tokio::test]
async fn provider_key_encrypted_at_rest() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("provider");

    let plaintext = "sk-secret-at-rest";
    let kp = kp();
    repo::insert_provider_key(
        &pool,
        &kp,
        &ProviderKey {
            id: "k1".into(),
            provider_id: "p1".into(),
            api_key: plaintext.into(),
            created_at: now().into(),
        },
    )
    .await
    .expect("insert encrypted key");

    // The ciphertext stored in the DB must NOT be the plaintext.
    let row: (Vec<u8>,) =
        sqlx::query_as("SELECT api_key_ciphertext FROM provider_key WHERE id = ?")
            .bind("k1")
            .fetch_one(&pool)
            .await
            .expect("read ciphertext column");
    assert_ne!(
        row.0,
        plaintext.as_bytes(),
        "ciphertext must not equal plaintext"
    );
    assert!(!row.0.is_empty(), "ciphertext must be non-empty");

    // The old plaintext column is gone (hard cutover).
    let has_api_key_col = sqlx::query("SELECT api_key FROM provider_key LIMIT 1")
        .fetch_all(&pool)
        .await
        .is_ok();
    assert!(
        !has_api_key_col,
        "api_key plaintext column must have been dropped"
    );

    // Round-trip: decrypt via list_provider_keys.
    let keys = repo::list_provider_keys(&pool, &kp)
        .await
        .expect("list + decrypt");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].api_key, plaintext);

    // A different master key fails to decrypt (tag failure → sqlx error).
    let wrong_kp = StaticKeyProvider::new([2u8; 32], 1);
    assert!(
        repo::list_provider_keys(&pool, &wrong_kp).await.is_err(),
        "wrong master key must fail to decrypt"
    );
}

/// T4.10 — provider_key_binding CRUD round-trip + CASCADE with its provider.
#[tokio::test]
async fn provider_key_binding_crud() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("seed provider");

    let b = hydra_core::model::ProviderKeyBinding {
        id: "b1".into(),
        key_prefix: "sk_aaa_".into(),
        provider_id: "p1".into(),
        enabled: true,
        created_at: now().into(),
        updated_at: now().into(),
    };
    repo::insert_provider_key_binding(&pool, &b)
        .await
        .expect("insert");
    assert_eq!(
        repo::get_provider_key_binding(&pool, "b1").await.unwrap(),
        b
    );

    let mut upd = b.clone();
    upd.key_prefix = "hk_bbb_".into();
    upd.enabled = false;
    repo::update_provider_key_binding(&pool, &upd)
        .await
        .expect("update");
    assert_eq!(
        repo::get_provider_key_binding(&pool, "b1").await.unwrap(),
        upd
    );

    let all = repo::list_provider_key_bindings(&pool).await.expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].key_prefix, "hk_bbb_");

    // CASCADE: deleting the provider removes its binding.
    repo::delete_provider(&pool, "p1").await.expect("delete p1");
    assert!(
        repo::get_provider_key_binding(&pool, "b1").await.is_err(),
        "binding must be CASCADE-deleted with its provider"
    );
}
