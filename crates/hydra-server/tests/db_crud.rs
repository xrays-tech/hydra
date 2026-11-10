//! Exhaustive CRUD, cascade, revision and cross-tenant coverage for `db::*`.
//!
//! This replaces what the SQLite `.sqlx` compile-time checking used to catch:
//! every public write/read pair is exercised against the selected real backend,
//! and every successful mutation must advance `config_revision` exactly once
//! while a no-op must not. Each test gets its own isolated schema/database via
//! [`common::setup_db`].

mod common;

use hydra_core::model::{
    BillingStatus, GlobalRole, LimitRole, Provider, ProviderKey, ProviderModel, Tenant,
    TenantAccessToken, TenantModel, TenantProvider, User, UserApiKey, UserGroup,
};
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::db::{self, ConfigVersion, Pool, TenantCert, TenantWrite};

fn kp() -> StaticKeyProvider {
    StaticKeyProvider::new([7u8; 32], 1)
}

/// A fixed, lexicographically ordered timestamp shape (`ORDER BY created_at`).
fn ts(second: u32) -> String {
    format!("2026-01-01 00:00:{second:02}")
}

async fn revision(pool: &Pool) -> u64 {
    match db::get_config_version(pool).await.expect("read revision") {
        ConfigVersion::Value(v) => v,
        other => panic!("expected a numeric config revision, got {other:?}"),
    }
}

fn provider(id: &str, key: &str, at: &str) -> Provider {
    Provider {
        id: id.into(),
        key: key.into(),
        name: format!("provider-{id}"),
        endpoint: "https://api.example.com".into(),
        weight: 1,
        created_at: at.into(),
        updated_at: at.into(),
        max_concurrency: Some(8),
        max_queue_depth: Some(16),
        queue_wait_timeout_ms: Some(2_000),
    }
}

fn provider_model(id: &str, key: &str, provider_id: &str, status: i32) -> ProviderModel {
    ProviderModel {
        id: id.into(),
        key: key.into(),
        name: format!("model-{id}"),
        provider_id: provider_id.into(),
        status,
    }
}

fn provider_key(id: &str, provider_id: &str, secret: &str, at: &str) -> ProviderKey {
    ProviderKey {
        id: id.into(),
        provider_id: provider_id.into(),
        api_key: secret.into(),
        created_at: at.into(),
    }
}

fn tenant(id: &str, domain: &str, at: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("tenant-{id}"),
        domain: domain.into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

fn tenant_provider(id: &str, tenant_id: &str, provider_id: &str) -> TenantProvider {
    TenantProvider {
        id: id.into(),
        tenant_id: tenant_id.into(),
        provider_id: provider_id.into(),
    }
}

fn tenant_model(id: &str, tenant_id: &str, model_key: &str) -> TenantModel {
    TenantModel {
        id: id.into(),
        tenant_id: tenant_id.into(),
        model_key: model_key.into(),
    }
}

fn limit_role(id: &str, matching_key: Option<&str>, at: &str) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: format!("role-{id}"),
        matching_key: matching_key.map(str::to_string),
        matching_model: Some("gpt-4".into()),
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(100),
        limit_token: Some(10_000),
        window: "m".into(),
        enabled: true,
        created_at: at.into(),
    }
}

fn group(id: &str, tenant_id: &str, name: &str, at: &str) -> UserGroup {
    UserGroup {
        id: id.into(),
        tenant_id: tenant_id.into(),
        name: name.into(),
        enabled: true,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

fn user(id: &str, tenant_id: &str, group_id: &str, at: &str) -> User {
    User {
        id: id.into(),
        tenant_id: tenant_id.into(),
        user_group_id: group_id.into(),
        external_id: None,
        name: format!("user-{id}"),
        enabled: true,
        billing_status: BillingStatus::Active,
        billing_reason: None,
        billing_updated_at: None,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

fn api_key(id: &str, user_id: &str, key_hash: &str, at: &str) -> UserApiKey {
    UserApiKey {
        id: id.into(),
        user_id: user_id.into(),
        name: format!("key-{id}"),
        key_hash: key_hash.into(),
        key_prefix: "hk_test".into(),
        key_last4: "last".into(),
        enabled: true,
        expires_at: None,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

fn access_token(id: &str, tenant_id: &str, hash: &str, at: &str) -> TenantAccessToken {
    TenantAccessToken {
        id: id.into(),
        tenant_id: tenant_id.into(),
        name: format!("token-{id}"),
        token_hash: hash.into(),
        token_prefix: "ht_test".into(),
        token_last4: "last".into(),
        enabled: true,
        expires_at: None,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

fn global_role(id: &str, model_key: &str, at: &str) -> GlobalRole {
    GlobalRole {
        id: id.into(),
        name: format!("role-{id}"),
        tenant_id: "*".into(),
        user_group_id: "*".into(),
        user_id: "*".into(),
        api_key_id: "*".into(),
        model_key: model_key.into(),
        provider_ids: Vec::new(),
        enabled: true,
        created_at: at.into(),
        updated_at: at.into(),
    }
}

#[tokio::test]
async fn provider_and_model_crud_and_ordering() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let mut rev = revision(pool).await;

    db::insert_provider(pool, &provider("p2", "later", &ts(1)))
        .await
        .expect("insert p2");
    rev += 1;
    assert_eq!(revision(pool).await, rev);
    db::insert_provider(pool, &provider("p1", "first", &ts(0)))
        .await
        .expect("insert p1");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    // Ordered by created_at, not id.
    let providers = db::list_providers(pool).await.expect("list providers");
    assert_eq!(
        providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec!["p1", "p2"]
    );
    let got = db::get_provider(pool, "p1").await.expect("get p1");
    assert_eq!(got.key, "first");
    assert_eq!(got.max_concurrency, Some(8));

    let mut p2 = got_for(&db::get_provider(pool, "p2").await);
    p2.name = "renamed".into();
    p2.weight = 0;
    p2.updated_at = ts(2);
    db::update_provider(pool, &p2).await.expect("update p2");
    rev += 1;
    assert_eq!(revision(pool).await, rev);
    let p2 = db::get_provider(pool, "p2").await.expect("get updated p2");
    assert_eq!(p2.name, "renamed");
    assert_eq!(p2.weight, 0);
    assert_eq!(p2.created_at, ts(1), "created_at must not change");

    db::insert_provider_model(pool, &provider_model("m-z", "zmodel", "p1", 1))
        .await
        .expect("insert z model");
    rev += 1;
    db::insert_provider_model(pool, &provider_model("m-a", "amodel", "p2", 0))
        .await
        .expect("insert a model");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    let models = db::list_provider_models(pool).await.expect("list models");
    assert_eq!(
        models.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
        vec!["amodel", "zmodel"],
        "models are ordered by (key, provider_id)"
    );
    let mut model = db::get_provider_model(pool, "m-a")
        .await
        .expect("get model");
    assert_eq!(model.status, 0);
    model.status = 1;
    db::update_provider_model(pool, &model)
        .await
        .expect("update model");
    rev += 1;
    assert_eq!(
        db::get_provider_model(pool, "m-a")
            .await
            .expect("get updated model")
            .status,
        1
    );

    db::delete_provider_model(pool, "m-z")
        .await
        .expect("delete model");
    rev += 1;
    assert!(db::get_provider_model(pool, "m-z").await.is_err());
    assert_eq!(revision(pool).await, rev);

    // No-op deletes must not advance the revision.
    db::delete_provider_model(pool, "missing")
        .await
        .expect("delete missing model is a no-op");
    db::delete_provider(pool, "missing")
        .await
        .expect("delete missing provider is a no-op");
    assert_eq!(revision(pool).await, rev, "no-op deletes must not bump");

    // Deleting a provider cascades its models.
    db::delete_provider(pool, "p2")
        .await
        .expect("delete p2 with child model");
    rev += 1;
    assert!(db::get_provider(pool, "p2").await.is_err());
    assert!(db::list_provider_models(pool)
        .await
        .expect("list models after cascade")
        .iter()
        .all(|m| m.provider_id != "p2"));
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

fn got_for<T>(value: &Result<T, sqlx::Error>) -> T
where
    T: Clone,
{
    value.as_ref().expect("query").clone()
}

#[tokio::test]
async fn provider_key_crud_and_sealing() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let kp = kp();
    let mut rev = revision(pool).await;

    db::insert_provider(pool, &provider("p1", "keyed", &ts(0)))
        .await
        .expect("insert provider");
    rev += 1;
    assert_eq!(revision(pool).await, rev);
    db::insert_provider_key(pool, &kp, &provider_key("k1", "p1", "sk-one", &ts(0)))
        .await
        .expect("insert k1");
    rev += 1;
    db::insert_provider_key(pool, &kp, &provider_key("k2", "p1", "sk-two", &ts(1)))
        .await
        .expect("insert k2");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    assert_eq!(
        db::get_provider_key(pool, &kp, "k1")
            .await
            .expect("get k1")
            .api_key,
        "sk-one"
    );
    assert_eq!(
        db::list_provider_keys(pool, &kp)
            .await
            .expect("list keys")
            .iter()
            .map(|k| k.id.as_str())
            .collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "keys are ordered by (provider_id, created_at, id)"
    );
    assert_eq!(
        db::list_provider_keys_by_provider(pool, &kp, "p1")
            .await
            .expect("list by provider")
            .len(),
        2
    );

    // At rest the column is ciphertext, never the plaintext.
    let raw: (Vec<u8>,) = sqlx::query_as(&db::dialect::sql(
        "SELECT api_key_ciphertext FROM provider_key WHERE id = ?",
    ))
    .bind("k1")
    .fetch_one(pool)
    .await
    .expect("raw ciphertext");
    assert_ne!(raw.0, b"sk-one".to_vec());

    db::upsert_provider_key(pool, &kp, &provider_key("k1", "p1", "sk-one-v2", &ts(0)))
        .await
        .expect("upsert k1");
    rev += 1;
    assert_eq!(
        db::get_provider_key(pool, &kp, "k1")
            .await
            .expect("get upserted k1")
            .api_key,
        "sk-one-v2"
    );
    assert_eq!(revision(pool).await, rev);

    db::delete_provider_key(pool, "k2")
        .await
        .expect("delete k2");
    rev += 1;
    assert!(db::get_provider_key(pool, &kp, "k2").await.is_err());
    db::delete_provider_key(pool, "missing")
        .await
        .expect("delete missing key is a no-op");
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

#[tokio::test]
async fn tenant_grants_and_certificate_crud() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let kp = kp();
    let mut rev = revision(pool).await;

    db::write_tenant(
        pool,
        &kp,
        TenantWrite {
            tenant: &tenant("t1", "one.example", &ts(0)),
            is_create: true,
            cert: Some((
                Some("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----".into()),
                Some("-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----".into()),
            )),
        },
    )
    .await
    .expect("write tenant + cert");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    let got = db::get_tenant(pool, "t1").await.expect("get tenant");
    assert_eq!(got.domain, "one.example");
    let cert = db::get_tenant_cert(pool, &kp, "t1")
        .await
        .expect("get cert")
        .expect("tenant exists");
    assert!(cert.cert_pem.as_deref().is_some_and(|p| p.contains("AAA")));
    assert!(cert
        .cert_key_pem
        .as_deref()
        .is_some_and(|p| p.contains("BBB")));
    assert_eq!(
        db::list_tenant_certs(pool, &kp).await.expect("certs").len(),
        1
    );

    let mut upd = got.clone();
    upd.name = "renamed".into();
    upd.enabled = false;
    upd.updated_at = ts(1);
    db::update_tenant(pool, &upd).await.expect("update tenant");
    rev += 1;
    let got = db::get_tenant(pool, "t1")
        .await
        .expect("get updated tenant");
    assert_eq!(got.name, "renamed");
    assert!(!got.enabled);

    db::update_tenant_cert(
        pool,
        &kp,
        &TenantCert {
            tenant_id: "t1".into(),
            cert_pem: Some("-----BEGIN CERTIFICATE-----\nCCC\n-----END CERTIFICATE-----".into()),
            cert_key_pem: None,
        },
    )
    .await
    .expect("replace cert");
    rev += 1;
    let cert = db::get_tenant_cert(pool, &kp, "t1")
        .await
        .expect("get replaced cert")
        .expect("tenant exists");
    assert!(cert.cert_pem.as_deref().is_some_and(|p| p.contains("CCC")));
    assert!(cert.cert_key_pem.is_none());
    assert_eq!(revision(pool).await, rev);

    db::insert_provider(pool, &provider("p1", "granted", &ts(0)))
        .await
        .expect("insert provider");
    rev += 1;
    db::insert_tenant_provider(pool, &tenant_provider("tp1", "t1", "p1"))
        .await
        .expect("insert grant");
    rev += 1;
    assert_eq!(
        db::get_tenant_provider(pool, "tp1")
            .await
            .expect("get grant")
            .provider_id,
        "p1"
    );
    assert_eq!(
        db::list_tenant_providers(pool).await.expect("grants").len(),
        1
    );

    db::insert_tenant_model(pool, &tenant_model("tm1", "t1", "gpt-4"))
        .await
        .expect("insert tenant model");
    rev += 1;
    assert_eq!(
        db::get_tenant_model(pool, "tm1")
            .await
            .expect("get tenant model")
            .model_key,
        "gpt-4"
    );
    assert_eq!(db::list_tenant_models(pool).await.expect("models").len(), 1);

    db::delete_tenant_provider(pool, "tp1")
        .await
        .expect("delete grant");
    rev += 1;
    db::delete_tenant_model(pool, "tm1")
        .await
        .expect("delete tenant model");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    db::delete_tenant(pool, "t1").await.expect("delete tenant");
    rev += 1;
    assert!(db::get_tenant(pool, "t1").await.is_err());
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

#[tokio::test]
async fn limit_role_crud_and_sealed_matching_key() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let kp = kp();
    let mut rev = revision(pool).await;

    db::insert_limit_role(pool, &kp, &limit_role("r2", Some("sk-two"), &ts(1)))
        .await
        .expect("insert r2");
    rev += 1;
    db::insert_limit_role(pool, &kp, &limit_role("r1", Some("sk-one"), &ts(0)))
        .await
        .expect("insert r1");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    let roles = db::list_limit_roles(pool, &kp).await.expect("list roles");
    assert_eq!(
        roles.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["r1", "r2"]
    );
    let got = db::get_limit_role(pool, &kp, "r1").await.expect("get r1");
    assert_eq!(got.matching_key.as_deref(), Some("sk-one"));

    // The bare column holds an envelope, not the client key.
    let raw: (Option<String>,) = sqlx::query_as(&db::dialect::sql(
        "SELECT matching_key FROM limit_role WHERE id = ?",
    ))
    .bind("r1")
    .fetch_one(pool)
    .await
    .expect("raw matching_key");
    let raw = raw.0.expect("sealed value");
    assert_ne!(raw, "sk-one");
    assert!(raw.starts_with("sealed:v1:"), "got {raw}");

    let mut upd = got.clone();
    upd.name = "renamed".into();
    upd.matching_key = Some("sk-one-v2".into());
    upd.window = "h".into();
    upd.limit_count = Some(5);
    db::update_limit_role(pool, &kp, &upd)
        .await
        .expect("update r1");
    rev += 1;
    let got = db::get_limit_role(pool, &kp, "r1")
        .await
        .expect("get updated r1");
    assert_eq!(got.matching_key.as_deref(), Some("sk-one-v2"));
    assert_eq!(got.window, "h");
    assert_eq!(got.limit_count, Some(5));
    assert_eq!(revision(pool).await, rev);

    db::delete_limit_role(pool, "r2").await.expect("delete r2");
    rev += 1;
    assert!(db::get_limit_role(pool, &kp, "r2").await.is_err());
    db::delete_limit_role(pool, "missing")
        .await
        .expect("delete missing role is a no-op");
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

#[tokio::test]
async fn user_hierarchy_crud_bulk_and_cross_tenant_denial() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let mut rev = revision(pool).await;

    db::insert_tenant(pool, &tenant("ta", "a.example", &ts(0)))
        .await
        .expect("tenant A");
    rev += 1;
    db::insert_tenant(pool, &tenant("tb", "b.example", &ts(1)))
        .await
        .expect("tenant B");
    rev += 1;

    db::upsert_user_group(pool, &group("ga", "ta", "A-group", &ts(0)))
        .await
        .expect("group A");
    rev += 1;
    db::upsert_user_group(pool, &group("gb", "tb", "B-group", &ts(1)))
        .await
        .expect("group B");
    rev += 1;
    db::upsert_user_groups_bulk(
        pool,
        &[
            group("ga2", "ta", "A-group-2", &ts(2)),
            group("ga3", "ta", "A-group-3", &ts(3)),
        ],
    )
    .await
    .expect("bulk groups");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    db::upsert_user(pool, &user("ua", "ta", "ga", &ts(0)))
        .await
        .expect("user A");
    rev += 1;
    db::upsert_user(pool, &user("ub", "tb", "gb", &ts(1)))
        .await
        .expect("user B");
    rev += 1;
    db::upsert_users_bulk(pool, &[user("ua2", "ta", "ga2", &ts(2))])
        .await
        .expect("bulk users");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    db::insert_user_api_key(pool, "ta", &api_key("ka", "ua", "hash-a", &ts(0)))
        .await
        .expect("key A");
    rev += 1;
    db::insert_user_api_key(pool, "tb", &api_key("kb", "ub", "hash-b", &ts(1)))
        .await
        .expect("key B");
    rev += 1;
    db::insert_user_api_keys_bulk(pool, "ta", &[api_key("ka2", "ua2", "hash-a2", &ts(2))])
        .await
        .expect("bulk keys");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    assert_eq!(db::list_user_groups(pool).await.expect("groups").len(), 4);
    assert_eq!(db::list_users(pool).await.expect("users").len(), 3);
    assert_eq!(db::list_user_api_keys(pool).await.expect("keys").len(), 3);

    // Cross-tenant writes are indistinguishable from unknown ids and change nothing.
    let cross_group = group("gb", "ta", "hijacked", &ts(9));
    assert!(matches!(
        db::upsert_user_group(pool, &cross_group).await,
        Err(sqlx::Error::RowNotFound)
    ));
    let b_group = db::list_user_groups(pool)
        .await
        .expect("groups")
        .into_iter()
        .find(|g| g.id == "gb")
        .expect("B group survives");
    assert_eq!(b_group.tenant_id, "tb");
    assert_eq!(b_group.name, "B-group");

    let cross_user = user("ub", "ta", "gb", &ts(9));
    assert!(matches!(
        db::upsert_user(pool, &cross_user).await,
        Err(sqlx::Error::RowNotFound)
    ));
    assert_eq!(
        db::list_users(pool)
            .await
            .expect("users")
            .into_iter()
            .find(|u| u.id == "ub")
            .expect("B user survives")
            .tenant_id,
        "tb"
    );

    assert!(matches!(
        db::insert_user_api_key(pool, "ta", &api_key("kb", "ub", "hash-x", &ts(9))).await,
        Err(sqlx::Error::RowNotFound)
    ));
    assert!(db::list_user_api_keys(pool)
        .await
        .expect("keys")
        .into_iter()
        .any(|k| k.id == "kb" && k.key_hash == "hash-b"));

    assert_eq!(
        db::delete_user_group(pool, "ta", "gb")
            .await
            .expect("cross-tenant group delete is a no-op"),
        0
    );
    assert_eq!(
        db::delete_user(pool, "ta", "ub")
            .await
            .expect("cross-tenant user delete is a no-op"),
        0
    );
    assert_eq!(
        db::delete_user_api_key(pool, "ta", "kb")
            .await
            .expect("cross-tenant key delete is a no-op"),
        0
    );
    assert_eq!(
        revision(pool).await,
        rev,
        "cross-tenant denials must not bump"
    );

    // A bulk batch containing one cross-tenant row aborts as a whole.
    let before_groups = db::list_user_groups(pool).await.expect("groups").len();
    let bad_batch = [
        group("ga4", "ta", "A-group-4", &ts(4)),
        group("gb", "ta", "hijack-bulk", &ts(4)),
    ];
    assert!(matches!(
        db::upsert_user_groups_bulk(pool, &bad_batch).await,
        Err(sqlx::Error::RowNotFound)
    ));
    assert_eq!(
        db::list_user_groups(pool).await.expect("groups").len(),
        before_groups,
        "a failed bulk write must roll back its valid rows too"
    );

    // Normal tenant-scoped deletes do work.
    assert_eq!(
        db::delete_user_api_key(pool, "ta", "ka2")
            .await
            .expect("delete key"),
        1
    );
    rev += 1;
    assert_eq!(
        db::delete_user(pool, "ta", "ua2")
            .await
            .expect("delete user"),
        1
    );
    rev += 1;
    assert_eq!(
        db::delete_user_group(pool, "ta", "ga3")
            .await
            .expect("delete group"),
        1
    );
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

#[tokio::test]
async fn tenant_access_token_crud() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let mut rev = revision(pool).await;

    db::insert_tenant(pool, &tenant("t1", "token.example", &ts(0)))
        .await
        .expect("tenant");
    rev += 1;
    db::insert_tenant_access_token(pool, &access_token("tok2", "t1", "hash-2", &ts(1)))
        .await
        .expect("token 2");
    rev += 1;
    db::insert_tenant_access_token(pool, &access_token("tok1", "t1", "hash-1", &ts(0)))
        .await
        .expect("token 1");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    let tokens = db::list_tenant_access_tokens(pool)
        .await
        .expect("list tokens");
    assert_eq!(
        tokens.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["tok1", "tok2"]
    );
    assert_eq!(
        db::delete_tenant_access_token(pool, "tok2")
            .await
            .expect("delete token"),
        1
    );
    rev += 1;
    assert_eq!(
        db::delete_tenant_access_token(pool, "missing")
            .await
            .expect("delete missing token is a no-op"),
        0
    );
    assert_eq!(revision(pool).await, rev);
    assert!(db::list_tenant_access_tokens(pool)
        .await
        .expect("tokens after delete")
        .iter()
        .all(|t| t.id != "tok2"));

    test_db.cleanup().await;
}

#[tokio::test]
async fn global_role_crud_provider_sets_and_bulk() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let mut rev = revision(pool).await;

    db::insert_provider(pool, &provider("p1", "pr-one", &ts(0)))
        .await
        .expect("provider p1");
    rev += 1;
    db::insert_provider(pool, &provider("p2", "pr-two", &ts(1)))
        .await
        .expect("provider p2");
    rev += 1;

    db::upsert_global_role_with_providers(
        pool,
        &global_role("r1", "gpt-4", &ts(0)),
        &["p2".into(), "p1".into(), "p1".into()],
    )
    .await
    .expect("role + provider set");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    let role = db::list_global_roles(pool)
        .await
        .expect("list roles")
        .into_iter()
        .find(|r| r.id == "r1")
        .expect("r1 exists");
    assert_eq!(role.provider_ids, vec!["p1", "p2"], "deduped + sorted");

    assert_eq!(
        db::replace_global_role_providers(pool, "r1", &["p2".into()])
            .await
            .expect("replace provider set"),
        1
    );
    rev += 1;
    let role = db::list_global_roles(pool)
        .await
        .expect("list after replace")
        .into_iter()
        .find(|r| r.id == "r1")
        .expect("r1 exists");
    assert_eq!(role.provider_ids, vec!["p2"]);

    let mut renamed = role.clone();
    renamed.name = "renamed-role".into();
    db::upsert_global_role(pool, &renamed)
        .await
        .expect("rewrite role row");
    rev += 1;
    let role = db::list_global_roles(pool)
        .await
        .expect("list after row update")
        .into_iter()
        .find(|r| r.id == "r1")
        .expect("r1 exists");
    assert_eq!(role.name, "renamed-role");
    assert_eq!(
        role.provider_ids,
        vec!["p2"],
        "row-only upsert must not touch the provider set"
    );

    db::upsert_global_roles_bulk(
        pool,
        &[
            (global_role("r2", "gpt-4", &ts(1)), vec!["p1".into()]),
            (global_role("r3", "claude", &ts(2)), vec!["p2".into()]),
        ],
    )
    .await
    .expect("bulk roles");
    rev += 1;
    let roles = db::list_global_roles(pool).await.expect("list bulk roles");
    assert_eq!(
        roles.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["r1", "r2", "r3"]
    );
    assert_eq!(revision(pool).await, rev);

    assert_eq!(
        db::delete_global_role(pool, "r2").await.expect("delete r2"),
        1
    );
    rev += 1;
    assert_eq!(
        db::delete_global_role(pool, "missing")
            .await
            .expect("delete missing role is a no-op"),
        0
    );
    assert_eq!(revision(pool).await, rev);
    assert!(db::list_global_roles(pool)
        .await
        .expect("roles after delete")
        .iter()
        .all(|r| r.id != "r2"));

    test_db.cleanup().await;
}

/// The MySQL regression: deleting a tenant that has child rows must succeed and
/// cascade. This is the `ER_NEED_REPREPARE` (1615) path on MySQL 8.0 and the
/// ordinary cascade path on PostgreSQL.
#[tokio::test]
async fn delete_tenant_cascades_all_child_rows() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let kp = kp();
    let mut rev = revision(pool).await;

    db::insert_provider(pool, &provider("p1", "cascade-provider", &ts(0)))
        .await
        .expect("provider");
    rev += 1;
    db::insert_provider_model(pool, &provider_model("pm", "m", "p1", 1))
        .await
        .expect("provider model");
    rev += 1;
    db::insert_provider_key(pool, &kp, &provider_key("pk", "p1", "sk-secret", &ts(0)))
        .await
        .expect("provider key");
    rev += 1;
    db::insert_tenant(pool, &tenant("t1", "cascade.example", &ts(0)))
        .await
        .expect("tenant");
    rev += 1;
    db::insert_tenant_provider(pool, &tenant_provider("tp", "t1", "p1"))
        .await
        .expect("tenant provider");
    rev += 1;
    db::insert_tenant_model(pool, &tenant_model("tm", "t1", "m"))
        .await
        .expect("tenant model");
    rev += 1;
    db::upsert_user_group(pool, &group("g", "t1", "G", &ts(0)))
        .await
        .expect("group");
    rev += 1;
    db::upsert_user(pool, &user("u", "t1", "g", &ts(0)))
        .await
        .expect("user");
    rev += 1;
    db::insert_user_api_key(pool, "t1", &api_key("k", "u", "hash-cascade", &ts(0)))
        .await
        .expect("user key");
    rev += 1;
    db::insert_tenant_access_token(pool, &access_token("tok", "t1", "token-hash", &ts(0)))
        .await
        .expect("access token");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    // Child rows exist before the delete; this is the shape that used to answer
    // MySQL 1615 from a cached prepared statement.
    assert_eq!(
        db::list_tenant_providers(pool).await.expect("grants").len(),
        1
    );
    assert_eq!(db::list_tenant_models(pool).await.expect("models").len(), 1);
    assert_eq!(db::list_user_groups(pool).await.expect("groups").len(), 1);
    assert_eq!(db::list_users(pool).await.expect("users").len(), 1);
    assert_eq!(db::list_user_api_keys(pool).await.expect("keys").len(), 1);
    assert_eq!(
        db::list_tenant_access_tokens(pool)
            .await
            .expect("tokens")
            .len(),
        1
    );

    db::delete_tenant(pool, "t1")
        .await
        .expect("delete tenant with child rows must succeed");
    rev += 1;
    assert_eq!(revision(pool).await, rev);

    assert!(db::get_tenant(pool, "t1").await.is_err());
    assert!(db::list_tenant_providers(pool)
        .await
        .expect("grants")
        .is_empty());
    assert!(db::list_tenant_models(pool)
        .await
        .expect("models")
        .is_empty());
    assert!(db::list_user_groups(pool).await.expect("groups").is_empty());
    assert!(db::list_users(pool).await.expect("users").is_empty());
    assert!(db::list_user_api_keys(pool).await.expect("keys").is_empty());
    assert!(db::list_tenant_access_tokens(pool)
        .await
        .expect("tokens")
        .is_empty());

    // Provider rows survive; deleting the provider cascades its own children.
    db::delete_provider(pool, "p1")
        .await
        .expect("delete provider with child rows");
    rev += 1;
    assert!(db::get_provider_model(pool, "pm").await.is_err());
    assert!(db::get_provider_key(pool, &kp, "pk").await.is_err());
    assert_eq!(revision(pool).await, rev);

    test_db.cleanup().await;
}

/// Every successful mutation advances the shared revision exactly once; a no-op
/// delete does not. This is the invariant the whole hot-reload gate rests on.
#[tokio::test]
async fn every_successful_write_bumps_revision_and_noop_does_not() {
    let test_db = common::setup_db().await;
    let pool = &test_db.pool;
    let kp = kp();
    assert_eq!(
        revision(pool).await,
        0,
        "a fresh database starts at revision 0"
    );

    db::insert_tenant(pool, &tenant("t1", "rev.example", &ts(0)))
        .await
        .expect("tenant");
    assert_eq!(revision(pool).await, 1);
    db::insert_provider(pool, &provider("p1", "rev-provider", &ts(0)))
        .await
        .expect("provider");
    assert_eq!(revision(pool).await, 2);
    db::insert_limit_role(pool, &kp, &limit_role("r1", None, &ts(0)))
        .await
        .expect("limit role");
    assert_eq!(revision(pool).await, 3);
    db::upsert_user_group(pool, &group("g", "t1", "G", &ts(0)))
        .await
        .expect("group");
    assert_eq!(revision(pool).await, 4);
    db::upsert_user(pool, &user("u", "t1", "g", &ts(0)))
        .await
        .expect("user");
    assert_eq!(revision(pool).await, 5);
    db::insert_user_api_key(pool, "t1", &api_key("k", "u", "rev-hash", &ts(0)))
        .await
        .expect("key");
    assert_eq!(revision(pool).await, 6);
    db::insert_tenant_access_token(pool, &access_token("tok", "t1", "rev-token", &ts(0)))
        .await
        .expect("token");
    assert_eq!(revision(pool).await, 7);
    db::upsert_global_role_with_providers(pool, &global_role("role", "*", &ts(0)), &["p1".into()])
        .await
        .expect("global role");
    assert_eq!(revision(pool).await, 8);

    let before = revision(pool).await;
    db::delete_tenant(pool, "missing")
        .await
        .expect("missing tenant");
    db::delete_provider(pool, "missing")
        .await
        .expect("missing provider");
    db::delete_user_group(pool, "t1", "missing")
        .await
        .expect("missing group");
    db::delete_user(pool, "t1", "missing")
        .await
        .expect("missing user");
    db::delete_user_api_key(pool, "t1", "missing")
        .await
        .expect("missing key");
    db::delete_limit_role(pool, "missing")
        .await
        .expect("missing role");
    db::delete_tenant_access_token(pool, "missing")
        .await
        .expect("missing token");
    db::delete_global_role(pool, "missing")
        .await
        .expect("missing role");
    assert_eq!(
        revision(pool).await,
        before,
        "no-op deletes must not advance the revision"
    );

    // The explicit bump / absolute-set helpers are the two other writers.
    assert_eq!(
        db::bump_config_revision(pool).await.expect("explicit bump"),
        before + 1
    );
    db::set_config_version(pool, 42)
        .await
        .expect("set absolute version");
    assert_eq!(revision(pool).await, 42);

    test_db.cleanup().await;
}

/// The config loader's paired read must be one snapshot on PostgreSQL too.
#[tokio::test]
async fn read_snapshot_is_repeatable_read() {
    let pool = common::setup_pool().await;
    let mut tx = db::begin_read_snapshot(&pool)
        .await
        .expect("begin read snapshot");

    #[cfg(feature = "db-postgres")]
    let isolation: String = sqlx::query_scalar("SELECT current_setting('transaction_isolation')")
        .fetch_one(&mut *tx)
        .await
        .expect("transaction isolation");
    #[cfg(feature = "db-mysql")]
    let isolation: String = sqlx::query_scalar("SELECT @@transaction_isolation")
        .fetch_one(&mut *tx)
        .await
        .expect("transaction isolation");

    tx.commit().await.expect("commit read snapshot");
    assert_eq!(
        isolation.to_ascii_lowercase().replace('-', " "),
        "repeatable read",
        "the loader must not run under READ COMMITTED; got {isolation:?}"
    );
}

/// A partitioned node must not hold the write lock forever: the session
/// guardrails are installed by `init_pool`'s `after_connect`.
#[tokio::test]
async fn session_guardrails_are_set() {
    let pool = common::setup_pool().await;

    #[cfg(feature = "db-postgres")]
    {
        let statement_timeout: String =
            sqlx::query_scalar("SELECT current_setting('statement_timeout')")
                .fetch_one(&pool)
                .await
                .expect("statement_timeout");
        let idle_timeout: String =
            sqlx::query_scalar("SELECT current_setting('idle_in_transaction_session_timeout')")
                .fetch_one(&pool)
                .await
                .expect("idle_in_transaction_session_timeout");
        assert_ne!(statement_timeout, "0", "statement_timeout must be bounded");
        assert_ne!(idle_timeout, "0", "idle transaction must be bounded");
    }

    #[cfg(feature = "db-mysql")]
    {
        let lock_wait: u64 = sqlx::query_scalar("SELECT @@innodb_lock_wait_timeout")
            .fetch_one(&pool)
            .await
            .expect("innodb_lock_wait_timeout");
        assert_eq!(lock_wait, 10, "the cluster write lock must time out");
    }
}

/// The write transaction bounds its own lock wait without touching migrations.
#[cfg(feature = "db-postgres")]
#[tokio::test]
async fn write_lock_wait_is_bounded() {
    let pool = common::setup_pool().await;
    let mut tx = db::begin_write(&pool).await.expect("begin write");
    let lock_timeout: String = sqlx::query_scalar("SELECT current_setting('lock_timeout')")
        .fetch_one(&mut *tx)
        .await
        .expect("lock_timeout inside a write transaction");
    tx.rollback().await.expect("rollback");
    assert_eq!(lock_timeout, "5s");
}
