//! Downstream listener setup: the config-derived topology, bindability probe,
//! TLS binding and the external startup self-check (F6 split of `main.rs`).
//!
//! The decision belongs to `hydra_server::listeners::plan`, a pure function of
//! DEPLOYMENT CONFIG: plaintext always, HTTPS iff `HYDRA_TLS_LISTEN` is set. The
//! tenant certs in the snapshot are *reported*, never consulted.

use std::sync::Arc;

use hydra_server::store::ConfigStore;
use tracing::{error, info};

/// Resolved cert store handed to the data plane. Its concrete type is
/// feature-dependent, exactly as in the original `main.rs`; single-node /
/// non-TLS builds never consult it.
#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
pub(crate) type CertStoreHandle = Option<Arc<hydra_server::tls::HydraCertStore>>;
#[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
pub(crate) type CertStoreHandle = Option<()>;

/// Build the proxy service, decide + probe the listener topology, and register
/// the service on the Pingora server.
#[cfg_attr(
    not(any(feature = "tls-boringssl", feature = "tls-openssl")),
    allow(unused_variables)
)]
pub(crate) fn setup_data_plane(
    server: &mut pingora_core::server::Server,
    app: hydra_server::proxy::HydraProxy,
    store: &ConfigStore,
    cert_store: CertStoreHandle,
    upgrade: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let listen_addr =
        std::env::var("HYDRA_LISTEN").unwrap_or_else(|_| super::env::DEFAULT_LISTEN.to_string());
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);

    // (3b) Downstream listener topology (design §12.1 / §15.1; 审核四 P2).
    //
    // The decision belongs to `listeners::plan`, a pure function of DEPLOYMENT
    // CONFIG: plaintext always, HTTPS iff `HYDRA_TLS_LISTEN` is set. The tenant
    // certs in the snapshot are *reported*, never consulted — writing one used
    // to flip the single listener to TLS, so the next restart took the plaintext
    // entry (80 → NodePort → pod :8080) down with RST while the process stayed
    // healthy and the probes stayed green:
    // dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md.
    let cert_count = store.snapshot().certs.len();
    let planned = hydra_server::listeners::plan(
        &listen_addr,
        hydra_server::listeners::tls_listen_from_env().as_deref(),
        hydra_server::listeners::tls_backend_available(),
        cert_count,
    )?;
    for note in &planned.notes {
        error!(kind = note.kind(), "{}", note.message());
        hydra_server::admin::metrics::record_listener_misconfig(note.kind());
    }
    let plan = planned.plan;

    // Bindability is established BEFORE Pingora: a bind failure inside Pingora's
    // service task is invisible from outside (its own log line is printed before
    // the bind, the retry loop only WARNs, and the final state is a process that
    // is alive with zero data-plane listeners while admin probes answer 200).
    // The plaintext entry is the availability path — if it cannot bind, refuse
    // to start instead of pretending to serve.
    //
    // EXCEPT in upgrade mode (`-u`): there the address is SUPPOSED to be in use —
    // the predecessor still holds it, and Pingora's `listen(fds)` reuses the socket
    // transferred over the upgrade socket (`pingora-core listeners/l4.rs:326`
    // looks the address up in the inherited FD table). Probing anyway aborted every
    // handover: measured 2026-09-29, the new process died with `Address already in
    // use … refusing to start` immediately after Pingora's own `Bootstrap done`, so
    // the documented `hydra -u` could never take the port over.
    if !upgrade {
        if let Err(e) = hydra_server::listeners::probe_bind(&plan.plain) {
            return Err(format!(
                "{}={}: {e}; refusing to start — the data plane would have no listener while the \
                 process still reported healthy",
                hydra_server::listeners::LISTEN_ENV,
                plan.plain
            )
            .into());
        }
    } else {
        info!(
            listener = %plan.plain,
            "upgrade mode: not probing the plaintext listener — the address is held by the \
             process we are taking it over from"
        );
    }
    proxy_service.add_tcp(&plan.plain);

    // HTTPS is optional and can never take the plaintext entry with it
    // (`Listeners::build()` is all-or-nothing per service, which is why a TLS
    // port that cannot bind used to be fatal for the whole data plane).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let tls_bound: Option<String> = match plan.tls.clone() {
        Some(tls_addr) => match if upgrade {
            // Same reasoning as the plaintext entry above.
            Ok(())
        } else {
            hydra_server::listeners::probe_bind(&tls_addr)
        } {
            Ok(()) => {
                // The store is built UNCONDITIONALLY whenever a TLS feature is
                // compiled in (`boot/state.rs::build_cert_store`, called from
                // `boot/mod.rs`), so this `Option` can only be `None` in a build
                // that cannot reach this line. Return the boot error instead of
                // panicking if that ever changes.
                let Some(cert_store) = cert_store else {
                    return Err("a TLS listener was configured but no certificate store \
                                was built; refusing to start"
                        .into());
                };
                match cert_store.build_tls_settings() {
                    Ok(settings) => {
                        proxy_service.add_tls_with_settings(&tls_addr, None, settings);
                        Some(tls_addr)
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            addr = %tls_addr,
                            "could not build downstream TLS settings; serving plaintext only \
                             (the TLS listener is NOT available)"
                        );
                        hydra_server::admin::metrics::record_listener_misconfig(
                            "tls_settings_failed",
                        );
                        None
                    }
                }
            }
            Err(e) => {
                error!(
                    error = %e,
                    addr = %tls_addr,
                    "could not bind the configured TLS listener; serving plaintext only"
                );
                hydra_server::admin::metrics::record_listener_misconfig("tls_bind_failed");
                None
            }
        },
        None => None,
    };
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let tls_bound: Option<String> = None;

    server.add_service(proxy_service);

    // Publish the effective listener set for `/api/v1/health` (the plan is
    // config-derived; `tls` reflects whether the HTTPS listener really came up).
    hydra_server::listeners::record_active(hydra_server::listeners::ActiveListeners {
        plain: plan.plain.clone(),
        tls: tls_bound.clone(),
        tls_configured: plan.tls.is_some(),
        tenant_certs: cert_count,
    });

    // Startup self-check: verify the entry port really accepts connections and
    // publish it as `hydra_listener_bound{protocol="plain"}`. It is the external
    // evidence the log line above cannot provide (see the function docs).
    spawn_listener_self_check(plan.plain.clone());
    // READ THIS GAUGE AS CONFIGURATION, NOT LIVENESS.
    //
    // `protocol="plain"` is the liveness signal: the self-check above dials the port
    // and rewrites it to false when the service never came up. `protocol="tls"` is
    // published here, from `tls_bound` — i.e. from the CONFIG decision — and nothing
    // ever revises it. That is deliberate rather than a gap: both addresses are
    // added to the SAME Pingora service (`add_tcp` above, `add_tls_with_settings`
    // below), and `Listeners::build()` is all-or-nothing per service, so a bind
    // failure (including the microsecond race between `probe_bind` and Pingora's own
    // bind) takes BOTH listeners down and is reported by `plain` going false. A
    // separate TLS dial would therefore be redundant. The pair of alerts in
    // `ops.md` §9.1 must be read the same way: `bound{plain} == 0` means the data
    // plane is not listening; `certs > 0 and bound{tls} == 0` means TLS was never
    // configured. (Its text says exactly that.)
    hydra_server::admin::metrics::record_listener_bound("tls", tls_bound.is_some());

    // Startup banner (bug report §5.5): one line that shows the whole protocol
    // shape, so every restart makes it obvious what is being served.
    info!(
        plain = %plan.plain,
        tls = tls_bound.as_deref().unwrap_or("disabled"),
        tenant_certs = cert_count,
        tls_backend = hydra_server::listeners::tls_backend_available(),
        "downstream listeners (config-derived): plaintext always, TLS only when {}=<addr> is set",
        hydra_server::listeners::TLS_LISTEN_ENV
    );

    Ok(())
}

/// Verify — from outside our own logging — that the plaintext entry port
/// actually accepts connections, and publish the result as
/// `hydra_listener_bound{protocol="plain"}`.
///
/// ## Why this exists
///
/// Our "listener bound" log line is printed *before* Pingora binds, so it is
/// not evidence. When the bind really fails, Pingora retries once a second for
/// 30 s and then panics **inside its service task**: the process stays alive and
/// keeps answering its health probes on the admin port while the data plane has
/// no listener at all (measured 2026-09-16; the probe paths then were the
/// token-free `/healthz` `/readyz` an `edge` served, both retired with the role —
/// what answers today is `/api/v1/health` with the admin token; `dev-docs/dev-plan.md`
/// 「监听拓扑与启动约定」). A connect attempt is the cheapest honest check.
///
/// The plaintext listener is the one probed: both listeners live in the same
/// Pingora service, whose `Listeners::build()` is all-or-nothing — if the TLS
/// address fails to bind, the plaintext accept loop never starts either. So a
/// successful connect to the entry port also proves the TLS listener was
/// configured successfully; `hydra_listener_bound{protocol="tls"}` is published
/// separately from the config-time decision.
fn spawn_listener_self_check(plain: String) {
    std::thread::spawn(move || {
        let Ok(addr) = plain.parse::<std::net::SocketAddr>() else {
            // `listeners::plan` already rejected unparseable addresses.
            return;
        };
        // 0.0.0.0 / :: are bind wildcards, not dialable destinations.
        let target = if addr.ip().is_unspecified() {
            match addr {
                std::net::SocketAddr::V4(_) => {
                    std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()))
                }
                std::net::SocketAddr::V6(_) => {
                    std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port()))
                }
            }
        } else {
            addr
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match std::net::TcpStream::connect_timeout(&target, std::time::Duration::from_secs(2)) {
                Ok(_) => {
                    hydra_server::admin::metrics::record_listener_bound("plain", true);
                    info!(
                        address = %plain,
                        "startup self-check: the plaintext entry port accepts connections"
                    );
                    return;
                }
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::debug!(address = %plain, error = %e, "self-check connect failed; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Err(e) => {
                    hydra_server::admin::metrics::record_listener_bound("plain", false);
                    error!(
                        address = %plain,
                        error = %e,
                        "startup self-check FAILED: nothing is listening on the plaintext entry port — \
                         the process looks healthy but serves no traffic (see hydra_listener_bound)"
                    );
                    return;
                }
            }
        }
    });
}
