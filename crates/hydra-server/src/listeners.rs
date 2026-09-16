//! Downstream listener topology (design §12.1 / §15.1) — the single owner of
//! "which local port speaks which transport protocol".
//!
//! ## The bug this module exists to prevent
//!
//! The topology used to be derived from the config **snapshot**: one address,
//! `add_tcp` when `snapshot.certs` was empty and `add_tls_with_settings` when
//! it was not. Writing a tenant certificate — a pure data change — therefore
//! flipped the transport protocol of the *only* data-plane port, silently, and
//! only at the next restart. Every restart then made the plaintext entry
//! (`80 → NodePort → pod :8080`) answer with RST while the process stayed
//! healthy and `/healthz` `/readyz` stayed green.
//! See `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`.
//!
//! ## KNOWN LIMITATION — same port, different bind addresses
//!
//! [`plan`] compares normalized address STRINGS, so `HYDRA_LISTEN=0.0.0.0:8080`
//! together with `HYDRA_TLS_LISTEN=127.0.0.1:8080` is **not** rejected
//! statically: the two spellings differ even though they overlap on
//! `127.0.0.1:8080`. This is deliberate — the same port on different interfaces
//! is a legitimate deployment on some hosts, so a stricter check would reject
//! working configurations — and it is covered at runtime instead: [`probe_bind`]
//! notices an unbindable address, and Pingora builds its services all-or-nothing,
//! so a genuine conflict surfaces as a startup failure or as the documented
//! TLS-bind degradation rather than as a half-configured process.
//! Recorded (and asserted) in `tests/boot_listeners.rs`.
//!
//! ## Contract (dev-plan 「监听拓扑与启动约定」)
//!
//! 1. The topology is a **pure function of deployment config** — never of the
//!    snapshot, the DB, or any tenant cert. That is why [`plan`] takes plain
//!    values and returns a plain value: it can be exhaustively tested without
//!    a process, a DB or a cert.
//! 2. The **plaintext listener is always bound**. It is the availability path
//!    the production entry maps to; nothing may take it away.
//! 3. The **TLS listener is bound iff [`TLS_LISTEN_ENV`] is set**, whether or
//!    not tenant certs exist yet — an operator who configures the port gets
//!    the port, and a cert written later lights up HTTPS without a restart
//!    (the cert store is wired from the start, see [`crate::tls`]).
//! 4. An illegal or conflicting address is a **startup error**, never a silent
//!    fallback; a cert that cannot be served is a **note** that is logged and
//!    counted, never a silent protocol change.
//!
//! [`TlsSettings`]: pingora_core::listeners::tls::TlsSettings

use std::net::SocketAddr;

/// Plaintext entry address (always bound).
pub const LISTEN_ENV: &str = "HYDRA_LISTEN";
/// Optional TLS address. Setting it — and only setting it — creates the HTTPS
/// listener.
pub const TLS_LISTEN_ENV: &str = "HYDRA_TLS_LISTEN";

/// What to listen on, decided from config alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerPlan {
    /// Plaintext HTTP entry — always present.
    pub plain: String,
    /// HTTPS listener, present iff the operator configured a port (and this
    /// binary has a TLS backend to speak it).
    pub tls: Option<String>,
}

/// A configuration combination that is legal but worth shouting about. These
/// are the two ways an operator ends up with certs that are not being served,
/// or a port that cannot complete a handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanNote {
    /// Tenants have certificates but no TLS port is configured: the per-tenant
    /// SNI feature is inert (HTTPS must be terminated upstream).
    CertsWithoutTlsPort { certs: usize },
    /// A TLS port is configured but no tenant certificate is loaded (yet):
    /// handshakes will fail until one arrives — without a restart, by design.
    TlsPortWithoutCerts,
}

impl PlanNote {
    /// Stable metric label value.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CertsWithoutTlsPort { .. } => "certs_without_tls_port",
            Self::TlsPortWithoutCerts => "tls_port_without_certs",
        }
    }

    /// Operator-facing explanation.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::CertsWithoutTlsPort { certs } => format!(
                "{certs} tenant cert(s) are configured but {TLS_LISTEN_ENV} is unset: this \
                 process serves plaintext only and the per-tenant SNI certificates are NOT \
                 being used. Set {TLS_LISTEN_ENV}=0.0.0.0:8443 (and point the entry at it) to \
                 serve HTTPS, or terminate TLS upstream."
            ),
            Self::TlsPortWithoutCerts => format!(
                "{TLS_LISTEN_ENV} is set but no tenant cert is loaded yet; TLS handshakes will \
                 fail until a certificate is written. No restart is needed once it is (the cert \
                 store follows the config snapshot)."
            ),
        }
    }
}

/// The plan plus the notes that go with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedListeners {
    pub plan: ListenerPlan,
    pub notes: Vec<PlanNote>,
}

/// Startup-fatal configuration errors. Every one of these used to be a silent
/// fallback (or simply unread, in the case of [`Self::NoTlsBackend`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("{var}={value:?} is not a valid listen address (expected IP:port): {reason}")]
    BadAddress {
        var: &'static str,
        value: String,
        reason: String,
    },
    #[error(
        "{LISTEN_ENV} and {TLS_LISTEN_ENV} both point at {addr}: the plaintext and TLS listeners \
         need different ports (a port cannot be both)"
    )]
    SameAddress { addr: String },
    #[error(
        "{TLS_LISTEN_ENV}={addr} is set but this binary was built without a downstream TLS \
         backend (neither `tls-boringssl` nor `tls-openssl`): the port would silently never \
         speak TLS. Rebuild with a TLS backend, or unset {TLS_LISTEN_ENV}."
    )]
    NoTlsBackend { addr: String },
}

/// Decide the listener topology from deployment configuration.
///
/// `tls_listen` is the raw [`TLS_LISTEN_ENV`] value (`None` = unset; an empty
/// or whitespace-only value counts as unset, because an empty env var is a
/// common templating artifact and "empty" cannot mean "port 0").
/// `tls_backend` says whether a downstream TLS backend was compiled in;
/// `certs` is only ever used to *describe* the outcome, never to choose it.
pub fn plan(
    plain: &str,
    tls_listen: Option<&str>,
    tls_backend: bool,
    certs: usize,
) -> Result<PlannedListeners, PlanError> {
    let plain = validate(LISTEN_ENV, plain)?;
    let tls = tls_listen
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| validate(TLS_LISTEN_ENV, v))
        .transpose()?;

    if let Some(tls) = tls {
        if tls == plain {
            return Err(PlanError::SameAddress {
                addr: plain.to_string(),
            });
        }
        if !tls_backend {
            return Err(PlanError::NoTlsBackend {
                addr: tls.to_string(),
            });
        }
    }

    let mut notes = Vec::new();
    match (tls.is_some(), certs) {
        (false, n) if n > 0 => notes.push(PlanNote::CertsWithoutTlsPort { certs: n }),
        (true, 0) => notes.push(PlanNote::TlsPortWithoutCerts),
        _ => {}
    }

    Ok(PlannedListeners {
        plan: ListenerPlan {
            plain: plain.to_string(),
            tls: tls.map(str::to_string),
        },
        notes,
    })
}

/// Parse and sanity-check one address. Port 0 is rejected: it would ask the
/// kernel for an arbitrary port, which no deployment entry can target.
fn validate<'a>(var: &'static str, value: &'a str) -> Result<&'a str, PlanError> {
    let value = value.trim();
    let bad = |reason: &str| PlanError::BadAddress {
        var,
        value: value.to_string(),
        reason: reason.to_string(),
    };
    let parsed: SocketAddr = value
        .parse()
        .map_err(|e: std::net::AddrParseError| bad(&e.to_string()))?;
    if parsed.port() == 0 {
        return Err(bad("port 0 selects an arbitrary port at bind time"));
    }
    Ok(value)
}

/// Can we bind this address right now?
///
/// This is the **evidence** half of the contract: our own "listener bound"
/// log line is printed before Pingora actually binds, and a bind failure there
/// ends as a panic inside Pingora's service task — the process keeps running
/// and keeps answering admin probes while the data plane has no listener at
/// all. Probing here turns that into a startup decision. The probe releases
/// the port immediately; Pingora binds it for real a moment later.
pub fn probe_bind(addr: &str) -> Result<(), String> {
    std::net::TcpListener::bind(addr)
        .map(|_| ())
        .map_err(|e| format!("cannot bind {addr}: {e}"))
}

/// Whether this binary can speak downstream TLS at all.
#[must_use]
pub const fn tls_backend_available() -> bool {
    cfg!(any(feature = "tls-boringssl", feature = "tls-openssl"))
}

/// What this process actually ended up listening on, published once at startup
/// so the admin API can answer "what is this node serving?" without a restart
/// or a log dive (bug report §5.5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActiveListeners {
    /// The plaintext entry that is bound.
    pub plain: String,
    /// The HTTPS listener that is bound, when one is.
    pub tls: Option<String>,
    /// Whether a TLS port was *configured* — `false` with certs present is the
    /// combination that leaves per-tenant SNI unused.
    pub tls_configured: bool,
    /// Tenant certs in the snapshot at startup (they no longer influence the
    /// topology; they are reported so the two can be compared).
    pub tenant_certs: usize,
}

static ACTIVE: std::sync::OnceLock<ActiveListeners> = std::sync::OnceLock::new();

/// Publish the effective listener set (called once by `main`).
pub fn record_active(listeners: ActiveListeners) {
    let _ = ACTIVE.set(listeners);
}

/// The effective listener set, when this process has published one.
#[must_use]
pub fn active() -> Option<&'static ActiveListeners> {
    ACTIVE.get()
}

/// Read [`TLS_LISTEN_ENV`] from the environment.
#[must_use]
pub fn tls_listen_from_env() -> Option<String> {
    std::env::var(TLS_LISTEN_ENV).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(
        plain: &str,
        tls: Option<&str>,
        backend: bool,
        certs: usize,
    ) -> Result<PlannedListeners, PlanError> {
        plan(plain, tls, backend, certs)
    }

    #[test]
    fn the_cert_count_never_changes_the_topology() {
        // THE regression: 0 certs and 3 certs must produce the same listeners.
        for certs in [0usize, 1, 3, 99] {
            let a = planned("0.0.0.0:8080", Some("0.0.0.0:8443"), true, 0)
                .expect("plan with a TLS port");
            let b = planned("0.0.0.0:8080", Some("0.0.0.0:8443"), true, certs)
                .expect("plan with a TLS port");
            assert_eq!(
                a.plan, b.plan,
                "tenant certs ({certs}) must not influence which listeners are bound"
            );
            assert_eq!(a.plan.plain, "0.0.0.0:8080");
            assert_eq!(a.plan.tls.as_deref(), Some("0.0.0.0:8443"));

            let c = planned("0.0.0.0:8080", None, true, 0).expect("plain-only plan");
            let d = planned("0.0.0.0:8080", None, true, certs).expect("plain-only plan");
            assert_eq!(c.plan, d.plan);
            assert_eq!(c.plan.tls, None);
        }
    }

    #[test]
    fn the_plaintext_entry_is_always_in_the_plan() {
        for (tls, certs) in [
            (None, 0usize),
            (None, 5),
            (Some("0.0.0.0:8443"), 0),
            (Some("0.0.0.0:8443"), 5),
        ] {
            let p = planned("127.0.0.1:8080", tls, true, certs).expect("plan");
            assert_eq!(p.plan.plain, "127.0.0.1:8080");
        }
    }

    #[test]
    fn the_tls_port_follows_the_config_not_the_certs() {
        // Configured, no certs yet → bound (certs may arrive later).
        let p = planned("0.0.0.0:8080", Some("0.0.0.0:8443"), true, 0).expect("plan");
        assert_eq!(p.plan.tls.as_deref(), Some("0.0.0.0:8443"));
        assert_eq!(p.notes, vec![PlanNote::TlsPortWithoutCerts]);

        // Not configured, certs present → NOT bound; shout about it.
        let p = planned("0.0.0.0:8080", None, true, 2).expect("plan");
        assert_eq!(p.plan.tls, None);
        assert_eq!(p.notes, vec![PlanNote::CertsWithoutTlsPort { certs: 2 }]);

        // Neither → nothing to say.
        let p = planned("0.0.0.0:8080", None, true, 0).expect("plan");
        assert!(p.notes.is_empty());
    }

    #[test]
    fn an_empty_tls_env_counts_as_unset() {
        for value in ["", "   "] {
            let p = planned("0.0.0.0:8080", Some(value), true, 0).expect("plan");
            assert_eq!(p.plan.tls, None, "{value:?} must not mean a TLS port");
        }
    }

    #[test]
    fn illegal_addresses_fail_startup() {
        for bad in ["", "8080", "0.0.0.0", "localhost:8443", "0.0.0.0:0"] {
            if bad.is_empty() {
                continue; // "" = unset for TLS (see an_empty_tls_env_counts_as_unset)
            }
            assert!(
                matches!(
                    planned("0.0.0.0:8080", Some(bad), true, 1),
                    Err(PlanError::BadAddress {
                        var: TLS_LISTEN_ENV,
                        ..
                    })
                ),
                "HYDRA_TLS_LISTEN={bad:?} must be rejected, not silently ignored"
            );
            assert!(
                matches!(
                    planned(bad, None, true, 0),
                    Err(PlanError::BadAddress {
                        var: LISTEN_ENV,
                        ..
                    })
                ),
                "HYDRA_LISTEN={bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn one_port_cannot_be_both_transports() {
        assert_eq!(
            planned("0.0.0.0:8080", Some("0.0.0.0:8080"), true, 0),
            Err(PlanError::SameAddress {
                addr: "0.0.0.0:8080".into()
            })
        );
    }

    #[test]
    fn a_tls_port_without_a_tls_backend_fails_startup() {
        // The alternative — accepting the setting and binding plaintext — is
        // exactly the silent degradation this module removes.
        assert_eq!(
            planned("0.0.0.0:8080", Some("0.0.0.0:8443"), false, 0),
            Err(PlanError::NoTlsBackend {
                addr: "0.0.0.0:8443".into()
            })
        );
    }

    #[test]
    fn addresses_are_trimmed() {
        let p = planned(" 0.0.0.0:8080 ", Some(" 0.0.0.0:8443 "), true, 0).expect("plan");
        assert_eq!(p.plan.plain, "0.0.0.0:8080");
        assert_eq!(p.plan.tls.as_deref(), Some("0.0.0.0:8443"));
    }

    #[test]
    fn misconfig_notes_carry_distinct_metric_labels() {
        assert_eq!(
            PlanNote::TlsPortWithoutCerts.kind(),
            "tls_port_without_certs"
        );
        assert_eq!(
            PlanNote::CertsWithoutTlsPort { certs: 1 }.kind(),
            "certs_without_tls_port"
        );
    }

    #[test]
    fn the_effective_listeners_are_readable_after_startup() {
        // What `/api/v1/health` reports: the config-derived outcome, so an
        // operator can tell "no TLS configured" from "TLS configured but not
        // bound" without reading logs.
        assert!(active().is_none(), "nothing is published before startup");
        record_active(ActiveListeners {
            plain: "0.0.0.0:8080".into(),
            tls: Some("0.0.0.0:8443".into()),
            tls_configured: true,
            tenant_certs: 1,
        });
        let active = active().expect("published");
        assert_eq!(active.plain, "0.0.0.0:8080");
        assert_eq!(active.tls.as_deref(), Some("0.0.0.0:8443"));
        assert!(active.tls_configured);
        assert_eq!(active.tenant_certs, 1);
    }
}
