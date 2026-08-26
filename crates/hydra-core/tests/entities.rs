//! T1.1 — `entities_derive_roundtrip`.
//!
//! Every entity and shared config type is constructed, serialised to JSON, and
//! deserialised back; the result must equal the original. This locks the serde
//! shape (field names, optional handling, enum tagging) for the Admin API and
//! the DB↔entity boundary in `hydra-server`.

use hydra_core::config::{CertMeta, ModelProvider};
use hydra_core::model::{
    Candidate, LimitRole, Provider, ProviderKey, ProviderKind, ProviderModel, RouteError, Tenant,
    TenantModel, TenantProvider, Usage, UsageRecord,
};
use hydra_core::rewrite::EndpointUrl;
use pretty_assertions::assert_eq;
use serde_json::json;

fn roundtrip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let ser = serde_json::to_string(value).expect("serialize");
    let de: T = serde_json::from_str(&ser).expect("deserialize");
    assert_eq!(value, &de, "roundtrip mismatch for serialized form:\n{ser}");
    de
}

#[test]
fn entities_derive_roundtrip() {
    let provider = Provider {
        id: "p_01".into(),
        key: "openai".into(),
        name: "OpenAI".into(),
        endpoint: "https://api.openai.com".into(),
        weight: 3,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-02T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    };
    roundtrip(&provider);

    let provider_model = ProviderModel {
        id: "pm_01".into(),
        key: "gpt-4o".into(),
        name: "GPT-4o".into(),
        provider_id: "p_01".into(),
        status: 1,
    };
    roundtrip(&provider_model);

    let provider_key = ProviderKey {
        id: "pk_01".into(),
        provider_id: "p_01".into(),
        api_key: "sk-secret".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    roundtrip(&provider_key);

    let tenant = Tenant {
        id: "t_01".into(),
        name: "Acme".into(),
        domain: "acme.com".into(),
        auth_url: "https://auth.acme.com/verify".into(),
        cert_key: Some("/certs/acme.key".into()),
        cert_file: Some("/certs/acme.crt".into()),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-02T00:00:00Z".into(),
    };
    roundtrip(&tenant);

    // Also verify the mandatory-auth_url invariant survives a missing cert pair.
    let tenant_bare = Tenant {
        cert_key: None,
        cert_file: None,
        enabled: false,
        auth_url: String::new(),
        ..tenant.clone()
    };
    roundtrip(&tenant_bare);

    let tenant_provider = TenantProvider {
        id: "tp_01".into(),
        tenant_id: "t_01".into(),
        provider_id: "p_01".into(),
    };
    roundtrip(&tenant_provider);

    let tenant_model = TenantModel {
        id: "tm_01".into(),
        tenant_id: "t_01".into(),
        model_key: "gpt-4o".into(),
    };
    roundtrip(&tenant_model);

    let limit_role = LimitRole {
        id: "lr_01".into(),
        name: "default".into(),
        matching_key: None,
        matching_model: Some("gpt-4o".into()),
        matching_tenant: Some("t_01".into()),
        matching_provider: None,
        limit_count: Some(100),
        limit_token: None,
        window: "m".into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    roundtrip(&limit_role);

    let candidate = Candidate {
        provider_id: "p_01".into(),
        endpoint: "https://api.openai.com:443".into(),
        weight: 3,
    };
    roundtrip(&candidate);

    // Enum roundtrips (external tagging).
    for variant in [
        RouteError::ModelNotAllowed,
        RouteError::ModelNotFound,
        RouteError::TenantForbidden,
        RouteError::NoAvailableProvider,
        RouteError::NoAvailableKey,
    ] {
        let back = roundtrip(&variant);
        assert_eq!(variant, back);
    }

    for variant in [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Generic,
    ] {
        let back = roundtrip(&variant);
        assert_eq!(variant, back);
    }

    // Usage: all-None (unknown) and fully-populated.
    let usage_empty = Usage::default();
    roundtrip(&usage_empty);

    let usage_full = Usage {
        tokens_in: Some(120),
        tokens_out: Some(80),
        cache_hit_tokens: Some(15),
    };
    let usage_back = roundtrip(&usage_full);
    assert_eq!(usage_full, usage_back);

    let usage_record = UsageRecord {
        tenant_id: "t_01".into(),
        provider_id: "p_01".into(),
        model_key: "gpt-4o".into(),
        client_api_key_masked: Some("sk-abcd…wxyz".into()),
        status_code: 200,
        tokens_in: Some(120),
        tokens_out: Some(80),
        cache_hit_tokens: Some(15),
        latency_ms: 1234,
        forward_latency_ms: Some(12),
        ttft_ms: Some(340),
        upstream_host: Some("api.openai.com".into()),
        error: None,
        trace_id: "trace-001".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    roundtrip(&usage_record);

    // Config-side value types.
    let model_provider = ModelProvider {
        provider_id: "p_01".into(),
        weight: 3,
    };
    roundtrip(&model_provider);

    let cert_meta = CertMeta {
        domain: "acme.com".into(),
        cert_file: Some("/certs/acme.crt".into()),
        cert_key: Some("/certs/acme.key".into()),
        cert_pem: None,
        cert_key_pem: None,
    };
    roundtrip(&cert_meta);

    // Content form (migration 0007): PEM content serialises; the private key
    // round-trips inside the snapshot (it is shipped sealed in the wire form;
    // here we only assert the plain type is serde-compatible).
    let cert_meta_content = CertMeta {
        domain: "acme.com".into(),
        cert_file: None,
        cert_key: None,
        cert_pem: Some("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into()),
        cert_key_pem: Some("-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----\n".into()),
    };
    roundtrip(&cert_meta_content);

    let endpoint = EndpointUrl {
        scheme: "https".into(),
        host: "api.openai.com".into(),
        port: 443,
        path_prefix: String::new(),
    };
    roundtrip(&endpoint);

    // Sanity: JSON shape is human-readable & field names are snake_case.
    assert_eq!(
        serde_json::to_value(&candidate).unwrap(),
        json!({
            "provider_id": "p_01",
            "endpoint": "https://api.openai.com:443",
            "weight": 3
        })
    );
    assert_eq!(
        serde_json::to_value(ProviderKind::Anthropic).unwrap(),
        json!("Anthropic")
    );
}
