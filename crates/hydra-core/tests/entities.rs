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

    // P2-10 / review A3: `api_key` is `skip_serializing` and REQUIRED on
    // deserialize, so `ProviderKey` is deliberately NOT serde-round-trippable —
    // the secret never leaves the process, and a body that omits it is a loud
    // error rather than a silently-defaulted empty credential. Both properties
    // are asserted in `secrets_are_never_serialized_and_required_on_absence`;
    // here we pin the wire shape only (no `api_key` in the serialized form).
    let provider_key = ProviderKey {
        id: "pk_01".into(),
        provider_id: "p_01".into(),
        api_key: "sk-must-not-appear".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let ser = serde_json::to_value(&provider_key).expect("serialize");
    assert_eq!(
        ser,
        json!({
            "id": "pk_01",
            "provider_id": "p_01",
            "created_at": "2026-01-01T00:00:00Z"
        }),
        "the serialized ProviderKey must never carry the plaintext api_key"
    );
    assert!(
        serde_json::from_value::<ProviderKey>(ser).is_err(),
        "the serialized form omits api_key on purpose, so it must not deserialize back"
    );

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

    // Content form (migration 0007): the public cert PEM round-trips. The
    // private key (cert_key_pem) is skip_serializing + default (P2-10) — never
    // serialized, so it defaults to None on a round-trip (asserted in
    // `secrets_are_never_serialized_and_default_on_absence`).
    let cert_meta_content = CertMeta {
        domain: "acme.com".into(),
        cert_file: None,
        cert_key: None,
        cert_pem: Some("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into()),
        cert_key_pem: None,
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

/// P2-10 — secret fields are NEVER serialized (they live only in memory / are
/// re-sealed at the DB boundary) and deserialize to their `Default` when absent.
/// This locks the `skip_serializing` + `default` contract for the provider
/// api-key (`ProviderKey::api_key`, a non-Option `String`) and the cert
/// private-key PEM (`CertMeta::cert_key_pem`, an `Option<String>`).
#[test]
fn secrets_are_never_serialized_and_required_on_absence() {
    // ProviderKey.api_key: skip on serialize, REQUIRED on deserialize.
    let provider_key = ProviderKey {
        id: "pk_01".into(),
        provider_id: "p_01".into(),
        api_key: "sk-secret".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let value = serde_json::to_value(&provider_key).unwrap();
    assert!(
        value.get("api_key").is_none(),
        "api_key must never be serialized (got: {value})"
    );
    // A payload that OMITS the field must be rejected, NOT defaulted to "":
    // `PUT /provider-keys/{id}` overwrites, so an empty default silently destroys
    // a working credential (review A3).
    let missing = serde_json::from_value::<ProviderKey>(json!({
        "id": "pk_01",
        "provider_id": "p_01",
        "created_at": "2026-01-01T00:00:00Z"
    }));
    assert!(
        missing.is_err(),
        "a payload without api_key must fail to deserialize, not default to an empty key"
    );
    // A payload that carries it still round-trips (explicit empty included — the
    // write boundary is what rejects empty values).
    let de: ProviderKey = serde_json::from_value(json!({
        "id": "pk_01",
        "provider_id": "p_01",
        "api_key": "sk-secret",
        "created_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(de.api_key, "sk-secret");
    assert_eq!(de.id, "pk_01");
    assert_eq!(de.provider_id, "p_01");

    // CertMeta.cert_key_pem: skip on serialize, default to None on absence.
    let cert_meta = CertMeta {
        domain: "acme.com".into(),
        cert_file: None,
        cert_key: None,
        cert_pem: Some("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into()),
        cert_key_pem: Some("-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----\n".into()),
    };
    let value = serde_json::to_value(&cert_meta).unwrap();
    assert!(
        value.get("cert_key_pem").is_none(),
        "cert_key_pem must never be serialized (got: {value})"
    );
    // The public cert PEM is still serialized (only the private key is skipped).
    assert!(
        value.get("cert_pem").is_some(),
        "cert_pem is public and must serialize"
    );
    // A payload without the field deserializes with the default (None).
    let de: CertMeta = serde_json::from_value(json!({
        "domain": "acme.com",
        "cert_pem": "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n"
    }))
    .unwrap();
    assert!(
        de.cert_key_pem.is_none(),
        "absent cert_key_pem defaults to None"
    );
}
