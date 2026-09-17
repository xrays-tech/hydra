//! T1.2 — `configdata_construct_and_index`.
//!
//! Hand-build a `ConfigData` (the server's loader would produce this from DB
//! rows) and assert every index is correct. This fixes the in-memory shape the
//! pure `router::resolve` and the loader (W2) will rely on.

use std::collections::{HashMap, HashSet};

use hydra_core::config::{CertMeta, ConfigData, ModelProvider};
use hydra_core::model::{LimitRole, Provider, Tenant};
use pretty_assertions::assert_eq;

fn sample_tenant() -> Tenant {
    Tenant {
        id: "t_acme".into(),
        name: "Acme".into(),
        domain: "acme.com".into(),
        auth_url: "https://auth.acme.com/verify".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn sample_provider(id: &str, key: &str, endpoint: &str, weight: i32) -> Provider {
    Provider {
        id: id.into(),
        key: key.into(),
        name: format!("{key} name"),
        endpoint: endpoint.into(),
        weight,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

#[test]
fn configdata_construct_and_index() {
    let mut cfg = ConfigData::default();

    // --- tenants_by_domain -------------------------------------------------
    let acme = sample_tenant();
    cfg.tenants_by_domain
        .insert(acme.domain.clone(), acme.clone());
    assert_eq!(
        cfg.tenants_by_domain.get("acme.com").map(|t| t.id.as_str()),
        Some("t_acme")
    );
    assert_eq!(
        cfg.tenants_by_domain
            .get("acme.com")
            .map(|t| t.auth_url.as_str()),
        Some("https://auth.acme.com/verify")
    );
    // Case-sensitivity contract: lookup is by lowercased domain.
    assert!(!cfg.tenants_by_domain.contains_key("ACME.COM"));

    // --- providers ---------------------------------------------------------
    let p_a = sample_provider("p_a", "openai", "https://api.openai.com", 3);
    let p_b = sample_provider("p_b", "azure", "https://gw.example.com/llm", 1);
    cfg.providers.insert(p_a.id.clone(), p_a.clone());
    cfg.providers.insert(p_b.id.clone(), p_b.clone());
    assert_eq!(cfg.providers.len(), 2);
    assert_eq!(
        cfg.providers.get("p_a").map(|p| p.endpoint.as_str()),
        Some("https://api.openai.com")
    );
    assert_eq!(cfg.providers.get("p_a").map(|p| p.weight), Some(3));

    // --- models_by_key (only status==1 online models land here) -----------
    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![
            ModelProvider {
                provider_id: "p_a".into(),
                weight: 3,
            },
            ModelProvider {
                provider_id: "p_b".into(),
                weight: 1,
            },
        ],
    );
    let gpt = cfg.models_by_key.get("gpt-4o").expect("gpt-4o indexed");
    assert_eq!(gpt.len(), 2);
    let provider_ids: HashSet<&str> = gpt.iter().map(|m| m.provider_id.as_str()).collect();
    assert!(provider_ids.contains("p_a"));
    assert!(provider_ids.contains("p_b"));
    assert_eq!(gpt[0].weight, 3);
    assert!(!cfg.models_by_key.contains_key("unknown-model"));

    // --- tenant_providers --------------------------------------------------
    let mut acme_providers = HashSet::new();
    acme_providers.insert("p_a".to_string());
    acme_providers.insert("p_b".to_string());
    cfg.tenant_providers.insert("t_acme".into(), acme_providers);
    let tp = cfg
        .tenant_providers
        .get("t_acme")
        .expect("tenant_providers indexed");
    assert!(tp.contains("p_a"));
    assert!(tp.contains("p_b"));
    assert!(!tp.contains("p_c"));

    // --- tenant_models (access gate) --------------------------------------
    let mut acme_models = HashSet::new();
    acme_models.insert("gpt-4o".to_string());
    cfg.tenant_models.insert("t_acme".into(), acme_models);
    let tm = cfg
        .tenant_models
        .get("t_acme")
        .expect("tenant_models indexed");
    assert!(tm.contains("gpt-4o"));
    assert!(!tm.contains("claude-3"));

    // --- provider_keys -----------------------------------------------------
    cfg.provider_keys
        .insert("p_a".into(), vec!["sk-a1".into(), "sk-a2".into()]);
    cfg.provider_keys.insert("p_b".into(), vec!["sk-b1".into()]);
    assert_eq!(cfg.provider_keys.get("p_a").map(|v| v.len()), Some(2));
    assert_eq!(
        cfg.provider_keys.get("p_b").map(|v| v.as_slice()),
        Some(&["sk-b1".to_string()][..])
    );

    // --- limit_roles -------------------------------------------------------
    cfg.limit_roles.push(LimitRole {
        id: "lr_01".into(),
        name: "per-minute".into(),
        matching_key: None,
        matching_model: Some("gpt-4o".into()),
        matching_tenant: Some("t_acme".into()),
        matching_provider: None,
        limit_count: Some(60),
        limit_token: None,
        window: "m".into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    assert_eq!(cfg.limit_roles.len(), 1);
    assert_eq!(cfg.limit_roles[0].id, "lr_01");
    assert_eq!(cfg.limit_roles[0].window, "m");

    // --- certs (plain map in core; server wraps in ArcSwap) ---------------
    cfg.certs.insert(
        "acme.com".into(),
        CertMeta {
            domain: "acme.com".into(),
            cert_file: None,
            cert_key: None,
            cert_pem: None,
            cert_key_pem: None,
        },
    );
    assert_eq!(
        cfg.certs.get("acme.com").map(|c| c.domain.as_str()),
        Some("acme.com")
    );

    // --- Default is fully empty (no panics, no stray entries) -------------
    let empty = ConfigData::default();
    assert!(empty.tenants_by_domain.is_empty());
    assert!(empty.tenants_by_id.is_empty());
    assert!(empty.models_by_key.is_empty());
    assert!(empty.tenant_providers.is_empty());
    assert!(empty.tenant_models.is_empty());
    assert!(empty.providers.is_empty());
    assert!(empty.provider_keys.is_empty());
    assert!(empty.limit_roles.is_empty());
    assert!(empty.certs.is_empty());

    // --- Clone is a deep, independent copy --------------------------------
    let mut clone = cfg.clone();
    clone.tenants_by_domain.clear();
    assert_eq!(
        cfg.tenants_by_domain.len(),
        1,
        "clone must be independent of original"
    );
    let _ = HashMap::<String, Provider>::new();
}

// ---------------------------------------------------------------------------
// tenants_by_id — derived index: one write entry point, and it stays off the wire
// ---------------------------------------------------------------------------

/// `reindex_tenants` is the ONLY writer, and it derives from
/// `tenants_by_domain` — including **replacing** whatever was there, so a
/// stale entry cannot survive a domain-index change.
#[test]
fn reindex_tenants_derives_from_the_domain_index_and_replaces() {
    let mut cfg = ConfigData::default();
    let acme = sample_tenant();
    cfg.tenants_by_domain
        .insert(acme.domain.clone(), acme.clone());

    // A hand-written entry that the domain index does NOT contain must not
    // survive: this is the "second write entry point" the design forbids.
    let mut ghost = sample_tenant();
    ghost.id = "t_ghost".into();
    ghost.domain = "ghost.example".into();
    cfg.tenants_by_id.insert(ghost.id.clone(), ghost);
    assert_eq!(cfg.tenants_by_id.len(), 1, "precondition");

    cfg.reindex_tenants();

    assert_eq!(cfg.tenants_by_id.len(), 1, "the ghost must be replaced");
    assert!(!cfg.tenants_by_id.contains_key("t_ghost"));
    assert_eq!(
        cfg.tenants_by_id.get("t_acme").map(|t| t.domain.as_str()),
        Some("acme.com")
    );

    // Removing the tenant from the domain index removes it here too.
    cfg.tenants_by_domain.clear();
    cfg.reindex_tenants();
    assert!(cfg.tenants_by_id.is_empty());
}

/// The index must NOT travel the cluster wire.
///
/// `ConfigData` is a field of `SnapshotWire` (`cluster/snapshot.rs`), so a
/// non-skipped field would (a) ship every tenant row twice per snapshot and
/// (b) break — or, with `serde(default)`, silently empty out — a mixed-version
/// deserialization. Silently emptying is the dangerous half: an empty
/// `tenants_by_id` turns a valid tenant token into `403 tenant_not_found`.
/// The replica rebuilds it in `hydrate` instead.
#[test]
fn tenants_by_id_is_not_serialized_and_deserializes_empty() {
    let mut cfg = ConfigData::default();
    let acme = sample_tenant();
    cfg.tenants_by_domain
        .insert(acme.domain.clone(), acme.clone());
    cfg.reindex_tenants();
    assert_eq!(
        cfg.tenants_by_id.len(),
        1,
        "precondition: populated in memory"
    );

    let json = serde_json::to_string(&cfg).expect("serialize");
    // 精确断言：wire 形态里**根本不存在** tenants_by_id 这个键。
    let as_value: serde_json::Value = serde_json::from_str(&json).expect("parse back");
    assert!(
        as_value.get("tenants_by_id").is_none(),
        "the derived index must not appear in the wire form: {json}"
    );
    assert!(
        as_value.get("tenants_by_domain").is_some(),
        "the source index MUST travel — the replica re-derives from it"
    );

    let back: ConfigData = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.tenants_by_domain.len(),
        1,
        "the SOURCE index does travel — it is what the replica re-derives from"
    );
    assert!(
        back.tenants_by_id.is_empty(),
        "the derived index must arrive empty; hydrate rebuilds it"
    );
    // ...and rebuilding from what DID travel restores it exactly.
    let mut back = back;
    back.reindex_tenants();
    assert_eq!(back.tenants_by_id, cfg.tenants_by_id);
}
