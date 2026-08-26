//! `HydraCertStore` — multi-tenant dynamic SNI certificate callback + the
//! cert-resolution pipeline (design §12).
//!
//! ## Single source of truth (design §12.1 / §5.2 P2-C7)
//!
//! The resolved certificate map lives in exactly one place: an
//! `Arc<ArcSwap<HashMap<String, ResolvedCert>>>` owned by [`HydraCertStore`].
//! [`HydraCertStore::resolve_and_store`] reads the `CertMeta` paths from a
//! config snapshot, PEM-parses them, and `store()`s the result into that same
//! ArcSwap. The TLS callback `load()`s it — so a reload is visible to the very
//! next handshake, with no second storage to drift. The map is keyed by
//! lowercase domain (the loader already lowercases tenant domains, §5.2).
//!
//! ## Isolation (design §5.4 / §12)
//!
//! A malformed cert/key for one tenant is skipped and logged at `WARN`; it
//! never breaks resolution of the other tenants' certs (T6.4).
//!
//! ## SNI selection (design §12.1)
//!
//! `certificate_callback` reads SNI via `ssl.servername(NameType::HOST_NAME)`,
//! looks up the cert (exact domain → first-level wildcard → default), and
//! applies it via `ext::ssl_use_certificate` / `ext::ssl_use_private_key`.
//!
//! ## SNI/Host mismatch (design §12.3)
//!
//! The cert is chosen by SNI; the tenant is resolved later by HTTP `Host`.
//! `handshake_complete_callback` stashes the SNI in the TLS digest extension;
//! [`observe_sni_host_mismatch`] (called once from the proxy request path)
//! compares it against the Host-derived domain and, on mismatch, increments
//! `hydra_sni_host_mismatch_total` + logs. It never blocks.
//!
//! This module is only compiled when a TLS backend (`tls-boringssl` /
//! `tls-openssl`) is enabled — the pingora cert/key/SNI types it touches exist
//! only under a real backend (under plain `proxy` the crate links the `noop_tls`
//! fallback module which lacks `x509`/`pkey`/`ssl`/`ext`).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};
use async_trait::async_trait;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::listeners::{TlsAccept, TlsAcceptCallbacks};
use pingora_core::protocols::tls::TlsRef;
use pingora_core::tls::ext;
use pingora_core::tls::pkey::{PKey, Private};
use pingora_core::tls::ssl::NameType;
use pingora_core::tls::x509::X509;
use prometheus::IntCounter;

use hydra_core::config::CertMeta;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised while resolving a tenant's `CertMeta` into a [`ResolvedCert`].
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("`cert_file` path missing for tenant domain '{0}'")]
    MissingCertPath(String),
    #[error("`cert_key` path missing for tenant domain '{0}'")]
    MissingKeyPath(String),
    #[error("reading cert file '{path}': {source}")]
    ReadCert {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("reading key file '{path}': {source}")]
    ReadKey {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing cert PEM for '{domain}': {reason}")]
    ParseCert { domain: String, reason: String },
    #[error("parsing key PEM for '{domain}': {reason}")]
    ParseKey { domain: String, reason: String },
}

// ---------------------------------------------------------------------------
// ResolvedCert — parsed cert + key ready for the callback (design §12.1)
// ---------------------------------------------------------------------------

/// A parsed server certificate + private key, ready to hand to the TLS
/// callback. Built once from a tenant's `cert_file`/`cert_key` PEM paths and
/// reused across handshakes.
///
/// The key is wrapped in `Arc` because `PKey<Private>` is not `Clone` (it owns
/// the secret material); the `Arc` makes [`ResolvedCert`] cheaply `Clone`-able
/// so the callback can pull a copy out of the shared map without a re-parse.
/// `X509` is already `Clone` (internally refcounted by the TLS library).
#[derive(Clone)]
pub struct ResolvedCert {
    /// The leaf certificate (parsed from `cert_file` PEM).
    pub cert: X509,
    /// The matching private key (parsed from `cert_key` PEM), shared via `Arc`.
    pub key: Arc<PKey<Private>>,
}

// ---------------------------------------------------------------------------
// Cert resolution: CertMeta (paths) -> ResolvedCert (parsed)  (design §12.1)
// ---------------------------------------------------------------------------

/// Resolve a `domain → CertMeta` map into `domain → ResolvedCert` by reading
/// and PEM-parsing each `cert_file`/`cert_key` pair (design §12.1).
///
/// **Per-tenant isolation (design §5.4 / §12, T6.4):** a bad cert/key for one
/// tenant is skipped and logged at `WARN`; the other tenants still resolve.
/// The returned map contains exactly the tenants that resolved successfully.
pub fn resolve_certs(certs: &HashMap<String, CertMeta>) -> HashMap<String, ResolvedCert> {
    let mut out = HashMap::with_capacity(certs.len());
    for (domain, meta) in certs {
        match resolve_one(domain, meta) {
            Ok(rc) => {
                out.insert(domain.clone(), rc);
            }
            Err(e) => {
                tracing::warn!(
                    target: "hydra::tls",
                    domain = %domain,
                    error = %e,
                    "cert resolution failed; this tenant's cert is skipped (others unaffected)"
                );
            }
        }
    }
    out
}

/// Resolve a single tenant's cert. Kept separate so the caller can isolate
/// failures per-tenant.
///
/// **Content-first (migration 0007 / cluster P0a):** when the snapshot carries
/// `cert_pem` / `cert_key_pem` (the multi-node form — PEM content shipped via
/// the config snapshot, no files), parse directly from memory with zero file
/// I/O. Legacy pre-0007 rows fall back to reading `cert_file` / `cert_key`
/// paths (single-node with files on disk).
fn resolve_one(domain: &str, meta: &CertMeta) -> Result<ResolvedCert, CertError> {
    if let (Some(cert_pem), Some(key_pem)) = (&meta.cert_pem, &meta.cert_key_pem) {
        let cert = X509::from_pem(cert_pem.as_bytes()).map_err(|e| CertError::ParseCert {
            domain: domain.to_string(),
            reason: e.to_string(),
        })?;
        let key =
            PKey::private_key_from_pem(key_pem.as_bytes()).map_err(|e| CertError::ParseKey {
                domain: domain.to_string(),
                reason: e.to_string(),
            })?;
        return Ok(ResolvedCert {
            cert,
            key: Arc::new(key),
        });
    }

    let cert_path = meta
        .cert_file
        .as_deref()
        .ok_or_else(|| CertError::MissingCertPath(domain.to_string()))?;
    let key_path = meta
        .cert_key
        .as_deref()
        .ok_or_else(|| CertError::MissingKeyPath(domain.to_string()))?;

    let cert_bytes = std::fs::read(cert_path).map_err(|source| CertError::ReadCert {
        path: cert_path.to_string(),
        source,
    })?;
    let key_bytes = std::fs::read(key_path).map_err(|source| CertError::ReadKey {
        path: key_path.to_string(),
        source,
    })?;

    let cert = X509::from_pem(&cert_bytes).map_err(|e| CertError::ParseCert {
        domain: domain.to_string(),
        reason: e.to_string(),
    })?;
    let key = PKey::private_key_from_pem(&key_bytes).map_err(|e| CertError::ParseKey {
        domain: domain.to_string(),
        reason: e.to_string(),
    })?;

    Ok(ResolvedCert {
        cert,
        key: Arc::new(key),
    })
}

// ---------------------------------------------------------------------------
// HydraCertStore — TlsAccept impl backed by the shared ArcSwap (design §12.1)
// ---------------------------------------------------------------------------

/// Multi-tenant dynamic SNI certificate callback. Holds the single source of
/// resolved certs (`certs`) and an optional fallback `default`. Cheap to
/// `Clone` (both fields are `Arc`/refcount-backed) so the pingora callback box
/// shares the exact same `ArcSwap` as the caller that drives hot-reload.
#[derive(Clone)]
pub struct HydraCertStore {
    /// `domain → ResolvedCert`, the single source. Resolution (`reload_all`)
    /// writes here via [`resolve_and_store`]; the TLS callback reads here.
    certs: Arc<ArcSwap<HashMap<String, ResolvedCert>>>,
    /// Fallback cert when no entry matches the SNI (design §12.1 `default`).
    default: Option<ResolvedCert>,
}

impl HydraCertStore {
    /// Build an empty store with an optional default cert. The caller fills
    /// `certs` via [`Self::resolve_and_store`] right after `ConfigStore::load`.
    #[must_use]
    pub fn new(default: Option<ResolvedCert>) -> Self {
        Self {
            certs: Arc::new(ArcSwap::new(Arc::new(HashMap::new()))),
            default,
        }
    }

    /// Resolve `CertMeta → ResolvedCert` and atomically store into the shared
    /// `ArcSwap` (the single source). Call this after `ConfigStore::load` and
    /// after every `ConfigStore::reload_all` so hot-reload is immediate
    /// (design §12.1): in-flight handshakes keep reading the previous map via
    /// their `ArcSwap` guard; the *next* handshake sees the new one.
    pub fn resolve_and_store(&self, certs: &HashMap<String, CertMeta>) {
        let resolved = resolve_certs(certs);
        self.certs.store(Arc::new(resolved));
    }

    /// Handle to the resolved-cert map (the single source). Exposed so callers
    /// and tests can observe/confirm what resolution wrote (T6.3).
    #[must_use]
    pub fn resolved(&self) -> Guard<Arc<HashMap<String, ResolvedCert>>> {
        self.certs.load()
    }

    /// Select a cert for `domain`: exact match → first-level wildcard → `None`.
    /// Borrowed-from-the-guard lookup path used by the callback.
    fn lookup(&self, domain: &str) -> Option<ResolvedCert> {
        let map = self.certs.load();
        if let Some(c) = map.get(domain) {
            return Some(c.clone());
        }
        // First-level wildcard: `foo.example.com` → `*.example.com`.
        if let Some(wild) = wildcard_of(domain) {
            if let Some(c) = map.get(&wild) {
                return Some(c.clone());
            }
        }
        None
    }

    /// Build the pingora [`TlsSettings`] registering `self` as the certificate
    /// callback, with HTTP/2 enabled (design §12.1:
    /// `TlsSettings::with_callbacks` + `enable_h2`). The boxed callback shares
    /// this store's `ArcSwap`, so later [`resolve_and_store`] calls take effect
    /// for new handshakes with no further wiring.
    ///
    /// Returns the pingora error directly so `main` can surface it.
    pub fn build_tls_settings(&self) -> pingora_core::Result<TlsSettings> {
        let cb: TlsAcceptCallbacks = Box::new(self.clone());
        let mut settings = TlsSettings::with_callbacks(cb)?;
        settings.enable_h2();
        Ok(settings)
    }
}

#[async_trait]
impl TlsAccept for HydraCertStore {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        let sni = ssl.servername(NameType::HOST_NAME);
        let resolved = sni
            .and_then(|d| self.lookup(d))
            .or_else(|| self.default.clone());
        let Some(c) = resolved else {
            tracing::warn!(
                target: "hydra::tls",
                sni = ?sni,
                "no cert matched SNI and no default configured; handshake will be rejected by the TLS layer"
            );
            return;
        };
        if let Err(e) = ext::ssl_use_certificate(ssl, &c.cert) {
            tracing::error!(target: "hydra::tls", error = %e, "ssl_use_certificate failed");
            return;
        }
        if let Err(e) = ext::ssl_use_private_key(ssl, &c.key) {
            tracing::error!(target: "hydra::tls", error = %e, "ssl_use_private_key failed");
        }
    }

    /// Capture the negotiated SNI into the TLS digest extension so the HTTP
    /// request path can run the §12.3 SNI/Host consistency check.
    async fn handshake_complete_callback(
        &self,
        ssl: &TlsRef,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        ssl.servername(NameType::HOST_NAME)
            .map(|s| Arc::new(s.to_string()) as Arc<dyn Any + Send + Sync>)
    }
}

/// Compute the first-level wildcard label for `domain`
/// (`foo.example.com` → `*.example.com`). Returns `None` when there is no
/// right-hand label to wildcard (e.g. `com`, or empty input).
fn wildcard_of(domain: &str) -> Option<String> {
    let rest = domain.split_once('.')?.1;
    if rest.is_empty() {
        return None;
    }
    Some(format!("*.{rest}"))
}

// ---------------------------------------------------------------------------
// §12.3 — SNI / Host mismatch observation (never blocks)
// ---------------------------------------------------------------------------

/// Metric name for SNI/Host mismatches (design §12.3).
const MISMATCH_METRIC: &str = "hydra_sni_host_mismatch_total";
const MISMATCH_HELP: &str = "TLS handshake SNI did not match the HTTP Host-derived tenant domain";

/// Process-wide counter, registered once with the default prometheus registry
/// (scraped by the W5 `/metrics` endpoint). Held in an `Option` so a registry
/// conflict can never panic the process — a failed registration simply hides
/// the metric (the `WARN` log still fires).
fn mismatch_counter() -> Option<&'static IntCounter> {
    use std::sync::OnceLock;
    static COUNTER: OnceLock<Option<IntCounter>> = OnceLock::new();
    COUNTER
        .get_or_init(|| prometheus::register_int_counter!(MISMATCH_METRIC, MISMATCH_HELP).ok())
        .as_ref()
}

/// Pure consistency check between the TLS SNI and the HTTP Host-derived domain
/// (design §12.3). Returns `true` when they are consistent (so no metric
/// increments):
///
/// - No SNI captured (`None`) → consistent: this is the plain-TCP / dev path or
///   a client that sent no SNI; the cert fallback handles it, never a mismatch.
/// - The Host port is stripped, lowercased; `localhost` / empty map to the
///   `localhost` tenant (§5.2 special case) and only mismatch an SNI that is
///   itself a non-localhost domain.
#[must_use]
pub fn sni_matches_host(sni: Option<&str>, host: &str) -> bool {
    let Some(sni) = sni else {
        return true;
    };
    let host_domain = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let host_lookup = if host_domain.is_empty() || host_domain == "localhost" {
        "localhost"
    } else {
        host_domain.as_str()
    };
    let sni = sni.to_ascii_lowercase();
    // localhost Host is consistent with any SNI over the dev listener.
    host_lookup == "localhost" || sni == host_lookup
}

/// Compare `sni`/`host` and, on mismatch, increment the metric + emit a `WARN`.
/// Pure-ish: takes the already-extracted SNI string (testable directly).
pub fn note_sni_host_mismatch(sni: Option<&str>, host: &str) {
    if sni_matches_host(sni, host) {
        return;
    }
    if let Some(counter) = mismatch_counter() {
        counter.inc();
    }
    tracing::warn!(
        target: "hydra::tls",
        sni = ?sni,
        host = %host,
        "SNI/Host mismatch: cert selected by SNI, tenant resolved by Host (not blocked)"
    );
}

/// Extract the SNI stashed by [`HydraCertStore::handshake_complete_callback`]
/// from the downstream TLS digest, then run the §12.3 check against `host`.
/// Intended as a single additive call from the proxy request path.
pub fn observe_sni_host_mismatch(session: &pingora_proxy::Session, host: &str) {
    let sni: Option<&str> = session
        .as_downstream()
        .digest()
        .and_then(|d| d.ssl_digest.as_ref())
        .and_then(|sd| sd.extension.get::<String>())
        .map(String::as_str);
    note_sni_host_mismatch(sni, host);
}

// ---------------------------------------------------------------------------
// Tests — pure helpers only (the integration TLS suite is in tests/tls.rs).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_first_label_only() {
        assert_eq!(wildcard_of("api.acme.com").as_deref(), Some("*.acme.com"));
        assert_eq!(
            wildcard_of("a.b.example.org").as_deref(),
            Some("*.b.example.org")
        );
        // No right-hand label.
        assert_eq!(wildcard_of("com"), None);
        assert_eq!(wildcard_of(""), None);
        assert_eq!(wildcard_of("foo."), None);
    }

    #[test]
    fn sni_match_logic() {
        // Exact match (case-insensitive, port stripped).
        assert!(sni_matches_host(Some("acme.com"), "acme.com"));
        assert!(sni_matches_host(Some("ACME.com"), "acme.com:443"));
        // Mismatch.
        assert!(!sni_matches_host(Some("acme.com"), "beta.io"));
        assert!(!sni_matches_host(Some("acme.com"), "beta.io:8443"));
        // No SNI → never a mismatch.
        assert!(sni_matches_host(None, "acme.com"));
        assert!(sni_matches_host(None, ""));
        // localhost Host → consistent with anything (dev listener).
        assert!(sni_matches_host(Some("acme.com"), "localhost"));
        assert!(sni_matches_host(Some("acme.com"), "localhost:8080"));
        assert!(sni_matches_host(Some("acme.com"), ""));
        // localhost SNI vs real Host → mismatch.
        assert!(!sni_matches_host(Some("localhost"), "acme.com"));
    }

    #[test]
    fn resolve_certs_isolates_bad_tenant() {
        // Good tenant points at a real fixture; bad tenant points at a garbage
        // file + a missing key path. Only the good one survives.
        let fixtures = env!("CARGO_MANIFEST_DIR").to_string() + "/tests/fixtures";
        let mut certs = HashMap::new();
        certs.insert(
            "acme.com".to_string(),
            CertMeta {
                domain: "acme.com".to_string(),
                cert_file: Some(format!("{fixtures}/acme.crt")),
                cert_key: Some(format!("{fixtures}/acme.key")),
                cert_pem: None,
                cert_key_pem: None,
            },
        );
        certs.insert(
            "broken.example".to_string(),
            CertMeta {
                domain: "broken.example".to_string(),
                cert_file: Some(format!("{fixtures}/bad.crt")),
                cert_key: Some(format!("{fixtures}/acme.key")),
                cert_pem: None,
                cert_key_pem: None,
            },
        );
        certs.insert(
            "nopath.example".to_string(),
            CertMeta {
                domain: "nopath.example".to_string(),
                cert_file: None,
                cert_key: None,
                cert_pem: None,
                cert_key_pem: None,
            },
        );

        let out = resolve_certs(&certs);
        assert!(out.contains_key("acme.com"), "good tenant resolves");
        assert!(
            !out.contains_key("broken.example"),
            "garbage PEM is skipped, not propagated"
        );
        assert!(
            !out.contains_key("nopath.example"),
            "missing paths are skipped"
        );
        assert_eq!(out.len(), 1, "only the good tenant remains");
    }

    #[test]
    fn store_then_lookup_roundtrip() {
        let fixtures = env!("CARGO_MANIFEST_DIR").to_string() + "/tests/fixtures";
        let mut certs = HashMap::new();
        certs.insert(
            "acme.com".to_string(),
            CertMeta {
                domain: "acme.com".to_string(),
                cert_file: Some(format!("{fixtures}/acme.crt")),
                cert_key: Some(format!("{fixtures}/acme.key")),
                cert_pem: None,
                cert_key_pem: None,
            },
        );
        let store = HydraCertStore::new(None);
        store.resolve_and_store(&certs);

        // The single source reflects what resolution wrote.
        let loaded = store.resolved();
        assert!(loaded.contains_key("acme.com"));

        // Exact lookup hits.
        assert!(store.lookup("acme.com").is_some());
        // Unknown domain misses (no default).
        assert!(store.lookup("evil.example").is_none());
    }

    #[test]
    fn resolve_certs_content_first_no_files() {
        // Migration 0007 form: PEM content shipped in the snapshot, zero file
        // I/O. This is the multi-node (shared-volume-free) resolution path.
        let fixtures = env!("CARGO_MANIFEST_DIR").to_string() + "/tests/fixtures";
        let cert_pem =
            std::fs::read_to_string(format!("{fixtures}/acme.crt")).expect("read fixture cert");
        let key_pem =
            std::fs::read_to_string(format!("{fixtures}/acme.key")).expect("read fixture key");

        let mut certs = HashMap::new();
        certs.insert(
            "acme.com".to_string(),
            CertMeta {
                domain: "acme.com".to_string(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some(cert_pem),
                cert_key_pem: Some(key_pem),
            },
        );
        // A content tenant whose PEM is garbage must be isolated, not fatal.
        certs.insert(
            "broken.example".to_string(),
            CertMeta {
                domain: "broken.example".to_string(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some("not a pem".to_string()),
                cert_key_pem: Some("not a key".to_string()),
            },
        );

        let out = resolve_certs(&certs);
        assert!(out.contains_key("acme.com"), "content tenant resolves");
        assert!(
            !out.contains_key("broken.example"),
            "garbage content PEM is skipped, not propagated"
        );
        assert_eq!(out.len(), 1, "only the good content tenant remains");
    }
}
