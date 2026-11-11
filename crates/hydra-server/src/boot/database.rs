//! Database boot: pool construction, migrations and startup-only maintenance
//! (F6 split of `main.rs`).

use hydra_server::{crypto, db};
use tracing::{error, info, warn};

use super::env::{
    enforce_db_tls, env_flag, reseal_requested, reseal_switch, ResealSwitch, DEFAULT_DB_URL,
};

/// (2a) DB pool + migrations. EVERY node has one (ADR-0001 D-2: a node that
/// cannot materialize the config tree into its own database cannot serve it).
///
/// The F3 transport check runs AFTER the URL is resolved (so the compiled-in
/// `DEFAULT_DB_URL` is covered too) and BEFORE any connection is attempted.
pub(crate) async fn init_pool() -> Result<db::Pool, Box<dyn std::error::Error>> {
    let db_url = std::env::var("HYDRA_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
    enforce_db_tls(&db_url, env_flag("HYDRA_DB_REQUIRE_TLS", 0)?)?;
    let pool = db::init_pool(&db_url).await?;
    db::run_migrate(&pool).await?;
    info!(db_url = %db_url, "database pool ready");
    Ok(pool)
}

/// (2b) One-shot master-key rotation (`HYDRA_RESEAL_SECRETS=1`) plus the
/// startup-only legacy limit-key sealing.
///
/// The rotation runs BEFORE the config store is loaded, because loading decrypts
/// provider keys and certificate private keys: with only the new key configured
/// those reads fail and the process refuses to start — which is exactly the
/// no-recovery hole this closes. Start once with
/// `HYDRA_ENCRYPTION_KEY=<new>` + `HYDRA_ENCRYPTION_KEY_VERSION=<new>` +
/// `HYDRA_ENCRYPTION_KEY_PREVIOUS=<old>` (+ `..._PREVIOUS_VERSION`), and this
/// re-seals every row in one transaction per table, prints a report and exits
/// instead of serving. Drop the previous-key variables afterwards.
///
/// A FAILED row is reported and left untouched, and the exit code is non-zero,
/// so an incomplete rotation cannot be mistaken for a finished one.
///
/// (2c) The loader is READ-ONLY, so the pre-D-16 plaintext
/// `limit_role.matching_key` rewrite happens here too.
pub(crate) async fn run_startup_maintenance(
    pool: &db::Pool,
    key_provider: &dyn crypto::KeyProvider,
) {
    if let ResealSwitch::Invalid(value) = reseal_switch() {
        // Fail LOUD: the alternative is a node that serves traffic while the operator believes
        // the rotation ran (measured: every unrecognised value used to do exactly that).
        error!(
            value = %value,
            "HYDRA_RESEAL_SECRETS is not a value this process understands: use 1/true/yes/on \
             to re-seal every stored secret and exit, or 0/false/no/off (or unset) to serve \
             normally; refusing to start rather than silently skipping the rotation"
        );
        std::process::exit(1);
    }
    if reseal_requested() {
        match db::reseal_secrets(pool, key_provider).await {
            Ok(report) => {
                println!(
                    "reseal: provider_keys={} tenant_certs={} limit_keys={} already_current={} \
                     failed={}",
                    report.provider_keys_resealed,
                    report.tenant_certs_resealed,
                    report.limit_keys_resealed,
                    report.already_current,
                    report.failed.len()
                );
                for f in &report.failed {
                    println!("reseal FAILED: {f}");
                }
                std::process::exit(if report.is_complete() { 0 } else { 1 });
            }
            Err(e) => {
                error!(error = %e, "reseal failed; nothing was rewritten");
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = db::seal_legacy_limit_keys(pool, key_provider).await {
        warn!(error = %e, "legacy limit-key sealing failed; serving existing rows");
    }
}
