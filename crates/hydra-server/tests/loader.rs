//! §2.3 — loader (`build_config`): row → ConfigData indexes + core validate.

mod common;

use std::collections::HashSet;

use hydra_core::config::{validate, Severity};
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
};
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::{db as repo, store::build_config};

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

fn tenant_full(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("{id}-tenant"),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: now().into(),
        updated_at: now().into(),
    }
}

/// Seed a coherent graph used by several loader tests: 2 providers (one with
/// weight 0 soft-disabled), an offline model, keys, a tenant with provider +
/// model grants.
async fn seed(pool: &sqlx::SqlitePool) {
    repo::insert_provider(pool, &provider("p1", "openai", 3))
        .await
        .expect("p1");
    repo::insert_provider(pool, &provider("p2", "azure", 7))
        .await
        .expect("p2");
    repo::insert_provider_model(pool, &model("m1", "gpt-4", "p1", 1))
        .await
        .expect("m1");
    repo::insert_provider_model(pool, &model("m2", "gpt-4", "p2", 1))
        .await
        .expect("m2");
    repo::insert_provider_model(pool, &model("m3", "gpt-4-off", "p1", 0))
        .await
        .expect("m3 offline");
    repo::insert_provider_key(
        pool,
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
        pool,
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
    repo::insert_tenant(pool, &tenant_full("t1", "acme.com"))
        .await
        .expect("t1");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("tp1");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp2".into(),
            tenant_id: "t1".into(),
            provider_id: "p2".into(),
        },
    )
    .await
    .expect("tp2");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("tm1");
}

/// T5.1 — `build_config` produces correct indexes.
#[tokio::test]
async fn loader_build_indexes_correct() {
    let pool = common::setup_pool().await;
    seed(&pool).await;

    let cfg = build_config(&pool, &kp()).await.expect("build");

    // providers
    assert_eq!(cfg.providers.len(), 2);
    assert_eq!(cfg.providers.get("p1").expect("p1").weight, 3);
    assert_eq!(cfg.providers.get("p2").expect("p2").weight, 7);

    // models_by_key: gpt-4 from p1 + p2 (online), gpt-4-off excluded.
    let gpt4 = cfg
        .models_by_key
        .get("gpt-4")
        .expect("gpt-4 candidates present");
    let gpt4_pids: HashSet<&str> = gpt4.iter().map(|m| m.provider_id.as_str()).collect();
    assert_eq!(gpt4_pids, HashSet::from(["p1", "p2"]));
    assert!(
        !cfg.models_by_key.contains_key("gpt-4-off"),
        "offline model key must not appear as its own index"
    );
    let p1_weight = gpt4
        .iter()
        .find(|m| m.provider_id == "p1")
        .expect("p1 in gpt-4")
        .weight;
    assert_eq!(p1_weight, 3, "weight must come from the provider");

    // provider_keys
    assert_eq!(
        cfg.provider_keys.get("p1").map(|v| v.len()),
        Some(1),
        "p1 should have one key"
    );

    // tenant_providers
    let tp = cfg
        .tenant_providers
        .get("t1")
        .expect("t1 provider grant set");
    assert_eq!(tp, &HashSet::from(["p1".to_string(), "p2".to_string()]));

    // tenant_models
    let tm = cfg.tenant_models.get("t1").expect("t1 model grant set");
    assert_eq!(tm, &HashSet::from(["gpt-4".to_string()]));

    // tenants_by_domain (lowercased)
    assert!(cfg.tenants_by_domain.contains_key("acme.com"));
    assert_eq!(
        cfg.tenants_by_domain["acme.com"].auth_url,
        "https://auth.acme.com/verify"
    );
}

/// T5.2 — `provider_model.status ∈ {0, -1}` is excluded from `models_by_key`.
/// Use distinct model keys (same `(key, provider_id)` would hit the UNIQUE
/// constraint) so each offline status is exercised independently.
#[tokio::test]
async fn loader_filters_offline_models() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("p1");
    repo::insert_provider_model(&pool, &model("m_on", "gpt-4", "p1", 1))
        .await
        .expect("online");
    repo::insert_provider_model(&pool, &model("m_off0", "manual-off", "p1", 0))
        .await
        .expect("manually offline (status=0)");
    repo::insert_provider_model(&pool, &model("m_off1", "probe-off", "p1", -1))
        .await
        .expect("probe offline (status=-1)");

    let cfg = build_config(&pool, &kp()).await.expect("build");

    // online gpt-4 present.
    let gpt4 = cfg.models_by_key.get("gpt-4").expect("online gpt-4");
    let pids: Vec<_> = gpt4.iter().map(|m| m.provider_id.as_str()).collect();
    assert_eq!(pids, vec!["p1"]);

    // both offline statuses are filtered out of the index entirely.
    assert!(
        !cfg.models_by_key.contains_key("manual-off"),
        "status=0 model must be filtered out"
    );
    assert!(
        !cfg.models_by_key.contains_key("probe-off"),
        "status=-1 model must be filtered out"
    );
}

/// T5.3 — domain is lowercased when indexing.
#[tokio::test]
async fn loader_lowercase_domain() {
    let pool = common::setup_pool().await;
    let mut t = tenant_full("t1", "Foo.COM");
    t.domain = "Foo.COM".into();
    repo::insert_tenant(&pool, &t).await.expect("insert");

    let cfg = build_config(&pool, &kp()).await.expect("build");
    assert!(
        cfg.tenants_by_domain.contains_key("foo.com"),
        "domain must be lowercased to 'foo.com'; got keys {:?}",
        cfg.tenants_by_domain.keys().collect::<Vec<_>>()
    );
    assert!(!cfg.tenants_by_domain.contains_key("Foo.COM"));
}

/// T5.4 — `domain="localhost"` enters the index normally.
#[tokio::test]
async fn loader_localhost_tenant() {
    let pool = common::setup_pool().await;
    repo::insert_tenant(&pool, &tenant_full("t_local", "localhost"))
        .await
        .expect("insert localhost tenant");

    let cfg = build_config(&pool, &kp()).await.expect("build");
    assert!(
        cfg.tenants_by_domain.contains_key("localhost"),
        "localhost tenant must be present in tenants_by_domain"
    );
}

/// T5.5 — dirty data flows through `core::validate` and surfaces issues.
///
/// The DB enforces hard FKs (`foreign_keys=ON`), so a dangling
/// `tenant_provider.provider_id` cannot be inserted. We instead exercise the
/// *soft* reference `tenant_model.model_key` (TEXT, no FK): a model_key that no
/// online provider serves is a real defect the pure validator must flag, and it
/// is DB-permitted (the model simply doesn't exist or is offline).
#[tokio::test]
async fn loader_runs_core_validate_warn() {
    let pool = common::setup_pool().await;
    repo::insert_tenant(&pool, &tenant_full("t1", "acme.com"))
        .await
        .expect("tenant");
    // Grant a model_key that no online provider serves.
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "ghost-model".into(),
        },
    )
    .await
    .expect("insert unserved tenant_model");

    let cfg = build_config(&pool, &kp())
        .await
        .expect("warn issues must not abort build");

    let issues = validate(&cfg);
    assert!(
        issues.iter().any(|i| {
            i.severity == Severity::Warn
                && i.message.contains("ghost-model")
                && i.message.contains("tenant_model")
        }),
        "expected a tenant_model 'no online provider' warning, got {issues:?}"
    );
}

/// T5.6 — cert_file/cert_key paths are carried into a `CertMeta` placeholder
/// keyed by (lowercased) domain. PEM parsing is W4's job.
#[tokio::test]
async fn loader_cert_meta_from_paths() {
    let pool = common::setup_pool().await;
    let mut t = tenant_full("t1", "secure.com");
    t.cert_file = Some("/etc/hydra/secure.crt".into());
    t.cert_key = Some("/etc/hydra/secure.key".into());
    repo::insert_tenant(&pool, &t).await.expect("insert");

    let cfg = build_config(&pool, &kp()).await.expect("build");
    let cert = cfg.certs.get("secure.com").expect("cert meta present");
    assert_eq!(cert.domain, "secure.com");
    assert_eq!(cert.cert_file.as_deref(), Some("/etc/hydra/secure.crt"));
    assert_eq!(cert.cert_key.as_deref(), Some("/etc/hydra/secure.key"));

    // A tenant without cert paths contributes no CertMeta.
    repo::insert_tenant(&pool, &tenant_full("t2", "plain.com"))
        .await
        .expect("plain tenant");
    let cfg2 = build_config(&pool, &kp()).await.expect("build2");
    assert!(!cfg2.certs.contains_key("plain.com"));
}

/// T5.7 (extra) — disabled limit roles are excluded from `ConfigData.limit_roles`.
#[tokio::test]
async fn loader_excludes_disabled_limit_roles() {
    let pool = common::setup_pool().await;
    let on = LimitRole {
        id: "lr_on".into(),
        name: "on".into(),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(10),
        limit_token: None,
        window: "m".into(),
        enabled: true,
        created_at: now().into(),
    };
    let off = LimitRole {
        id: "lr_off".into(),
        name: "off".into(),
        enabled: false,
        ..on.clone()
    };
    repo::insert_limit_role(&pool, &on).await.expect("on");
    repo::insert_limit_role(&pool, &off).await.expect("off");

    let cfg = build_config(&pool, &kp()).await.expect("build");
    assert_eq!(cfg.limit_roles.len(), 1);
    assert_eq!(cfg.limit_roles[0].id, "lr_on");
}

#[tokio::test]
async fn load_key_prefix_bindings_enabled_only() {
    let pool = common::setup_pool().await;
    seed(&pool).await;
    let mk = |id: &str, prefix: &str, provider_id: &str, enabled: bool| {
        hydra_core::model::ProviderKeyBinding {
            id: id.into(),
            key_prefix: prefix.into(),
            provider_id: provider_id.into(),
            enabled,
            created_at: now().into(),
            updated_at: now().into(),
        }
    };
    repo::insert_provider_key_binding(&pool, &mk("b1", "sk_aaa_", "p1", true))
        .await
        .expect("b1");
    repo::insert_provider_key_binding(&pool, &mk("b2", "hk_", "p2", false))
        .await
        .expect("b2");

    let cfg = build_config(&pool, &kp()).await.expect("build_config");
    assert_eq!(cfg.key_prefix_bindings.len(), 1, "only enabled rows load");
    assert_eq!(cfg.key_prefix_bindings[0].id, "b1");
    assert_eq!(cfg.key_prefix_bindings[0].provider_id, "p1");
}
