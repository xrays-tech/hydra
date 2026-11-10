//! Master-key rotation and legacy-secret backfills.

use crate::crypto::{self, KeyProvider, Sealed};

use super::dialect;
use super::limit_role::{legacy_limit_key_rows, reseal_limit_key_row};
use super::pool::{begin_write, bump_config_revision_on_conn};
use super::rows::crypto_to_sqlx;
use super::tenant::{list_tenant_certs, list_tenants, update_tenant_cert, TenantCert};
use super::{Pool, Tx};

/// One-time backfill for migration 0007: tenants with legacy `cert_file` /
/// `cert_key` paths but no stored content get their files read on this node
/// and stored as content (so the DB becomes self-contained and the shared
/// cert volume can be dropped). Best-effort: a failure is logged per tenant
/// and never aborts startup — the path fallback keeps serving until the files
/// become readable or the operator re-enters the cert via the admin API.
pub async fn backfill_legacy_certs(pool: &Pool, kp: &dyn KeyProvider) {
    let tenants = match list_tenants(pool).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "cert backfill: list_tenants failed; skipping");
            return;
        }
    };
    let existing = match list_tenant_certs(pool, kp).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cert backfill: list_tenant_certs failed; skipping");
            return;
        }
    };
    let with_content: std::collections::HashSet<&str> = existing
        .iter()
        .filter(|c| c.cert_pem.is_some())
        .map(|c| c.tenant_id.as_str())
        .collect();

    for t in tenants {
        if with_content.contains(t.id.as_str()) {
            continue;
        }
        let (Some(cert_path), Some(key_path)) = (&t.cert_file, &t.cert_key) else {
            continue;
        };
        let (Ok(cert_bytes), Ok(key_bytes)) = (std::fs::read(cert_path), std::fs::read(key_path))
        else {
            tracing::warn!(
                tenant = %t.id,
                cert_path,
                key_path,
                "cert backfill: legacy cert paths not readable on this node; path fallback remains active"
            );
            continue;
        };
        let cert = TenantCert {
            tenant_id: t.id.clone(),
            cert_pem: Some(String::from_utf8_lossy(&cert_bytes).into_owned()),
            cert_key_pem: Some(String::from_utf8_lossy(&key_bytes).into_owned()),
        };
        match update_tenant_cert(pool, kp, &cert).await {
            Ok(()) => tracing::info!(tenant = %t.id, "cert backfill: legacy paths -> content"),
            Err(e) => tracing::warn!(tenant = %t.id, error = %e, "cert backfill: store failed"),
        }
    }
}

/// Seal every `limit_role.matching_key` still stored as PLAINTEXT. Returns how many rows were
/// rewritten (0 on a database that is already sealed).
///
/// ## Why this exists, and why it is not inside the reader
///
/// Decision D-16 says an upgrade needs no manual step. The rows written before that decision hold a
/// client key in the clear, so something has to rewrite them — and that something must be a WRITE,
/// with its own transaction, not a side effect of a read (see [`crate::db::list_limit_roles_on`]).
///
/// ## Shape
///
/// 1. [`legacy_limit_key_rows`] reads first, with no lock: on a sealed database this function opens
///    no write transaction at all, which is what keeps "the loader ran" from meaning "the loader took
///    the write lock" on every reload.
/// 2. Each write is [`reseal_limit_key_row`]'s compare-and-swap, so an admin write (or another node)
///    landing between the read and the write wins and this pass affects nothing.
/// 3. Only rows that actually changed are counted, so the caller's log line means what it says.
///
/// # Errors
/// A [`CryptoError`] from sealing, or a SQL error from the update.
pub async fn seal_legacy_limit_keys(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<usize, sqlx::Error> {
    let pending = legacy_limit_key_rows(pool).await?;
    if pending.is_empty() {
        return Ok(0); // already sealed: not even a write transaction is opened
    }

    let mut tx = begin_write(pool).await?;
    let mut resealed = 0usize;
    for (id, value) in pending {
        let fresh = kp.seal(value.as_bytes()).map_err(crypto_to_sqlx)?.to_text();
        resealed += reseal_limit_key_row(&mut *tx, &id, &value, &fresh).await? as usize;
    }
    if resealed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(resealed)
}

/// What a re-seal pass did (see [`reseal_secrets`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResealReport {
    /// Provider api-keys rewritten under the current key version.
    pub provider_keys_resealed: usize,
    /// Tenant certificate private keys rewritten under the current key version.
    pub tenant_certs_resealed: usize,
    /// `limit_role.matching_key` values rewritten under the current key version (decision D-16;
    /// includes pre-D-16 PLAINTEXT rows, which this pass seals for the first time).
    pub limit_keys_resealed: usize,
    /// Rows already at the current version (nothing to do).
    pub already_current: usize,
    /// Rows that could NOT be opened (unknown version / wrong key), left
    /// untouched. Non-empty means the rotation is INCOMPLETE.
    pub failed: Vec<String>,
}

impl ResealReport {
    /// True when every stored secret now opens under the current key version.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Re-encrypt every stored secret under the provider's CURRENT key version.
///
/// This is the missing half of master-key rotation. Before it existed, changing
/// `HYDRA_ENCRYPTION_KEY` made every ciphertext unopenable, so
/// `ConfigStore::load` failed and the process refused to start — and the admin
/// API that could have re-entered the keys needs that same process, so there was
/// no way back in short of editing the database by hand.
///
/// The procedure is: start once with the new key as
/// `HYDRA_ENCRYPTION_KEY` (+ `HYDRA_ENCRYPTION_KEY_VERSION`), the old key as
/// `HYDRA_ENCRYPTION_KEY_PREVIOUS` (+ `..._PREVIOUS_VERSION`) and
/// `HYDRA_RESEAL_SECRETS=1`. Every row is read AND rewritten inside ONE write
/// transaction, then the previous key can be dropped. Rows that cannot be opened
/// are REPORTED and left alone (never silently rewritten with garbage), and the
/// caller must exit non-zero so nobody believes an incomplete rotation succeeded.
///
/// Reads happen through the transaction on purpose: reading them from the pool
/// first would (a) read a different snapshot than the one being written and
/// (b) deadlock on a single-connection pool (`:memory:` in tests), which is how
/// this was caught.
/// Re-seal one `(id, ciphertext, nonce, version)` table under the current key
/// version, returning the counts and failures for `reseal_secrets` to fold in.
///
/// A row whose version already equals the current one is NOT automatically
/// "already current". The version is an INDEPENDENT knob from the key
/// MATERIAL: rotate the key and forget to bump
/// `HYDRA_ENCRYPTION_KEY_VERSION` and every row still carries the old
/// number while the ring's slot for that number now holds different bytes.
/// Counting such a row as fine (and exiting 0) is the worst outcome in this
/// file, because the operator's next step is to delete the only key that can
/// open it — after which the process refuses to start with no way back. So
/// the claim is VERIFIED: the row must actually open under the current key,
/// which is what "already current" is supposed to mean.
#[allow(clippy::too_many_arguments)]
async fn reseal_column(
    tx: &mut Tx<'_>,
    kp: &dyn KeyProvider,
    current: u32,
    table: &str,
    id_label: &str,
    id_suffix: &str,
    ct_col: &str,
    nonce_col: &str,
    ver_col: &str,
    selection: &str,
) -> Result<(usize, usize, Vec<String>), sqlx::Error> {
    let mut resealed = 0usize;
    let mut already_current = 0usize;
    let mut failed = Vec::new();

    let select =
        format!("SELECT id, {ct_col}, {nonce_col}, {ver_col} FROM {table}{selection} ORDER BY id");
    let rows: Vec<(String, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(&dialect::sql(&select))
        .fetch_all(&mut **tx)
        .await?;
    for (id, ct, nonce, version) in rows {
        let subject = format!("{id_label} {id}{id_suffix}");
        let version = version as u32;
        if version == current {
            match <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) {
                Ok(nonce) => {
                    let sealed = Sealed {
                        ciphertext: ct,
                        nonce,
                        key_version: version,
                    };
                    if kp.open(&sealed).is_ok() {
                        already_current += 1;
                        continue;
                    }
                    failed.push(format!(
                        "{subject}: labelled key_version {version} (the CURRENT \
                         version) but it does NOT open under the current key — the key was \
                         most likely rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. \
                         Do NOT delete the previous key until this is resolved"
                    ));
                }
                Err(_) => failed.push(format!("{subject}: nonce has the wrong length")),
            }
            continue;
        }
        let Ok(nonce) = <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) else {
            failed.push(format!("{subject}: nonce has the wrong length"));
            continue;
        };
        let sealed = Sealed {
            ciphertext: ct,
            nonce,
            key_version: version,
        };
        match kp.open(&sealed) {
            Ok(plaintext) => match kp.seal(&plaintext) {
                Ok(fresh) => {
                    let update = format!(
                        "UPDATE {table} SET {ct_col} = ?, {nonce_col} = ?, {ver_col} = ? \
                         WHERE id = ?"
                    );
                    sqlx::query(&dialect::sql(&update))
                        .bind(&fresh.ciphertext)
                        .bind(fresh.nonce.to_vec())
                        .bind(i64::from(fresh.key_version))
                        .bind(&id)
                        .execute(&mut **tx)
                        .await?;
                    resealed += 1;
                }
                Err(e) => failed.push(format!("{subject}: seal failed: {e}")),
            },
            Err(e) => failed.push(format!("{subject}: cannot open key_version {version}: {e}")),
        }
    }
    Ok((resealed, already_current, failed))
}

pub async fn reseal_secrets(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<ResealReport, sqlx::Error> {
    let mut report = ResealReport::default();
    let current = kp.version();

    let mut tx = begin_write(pool).await?;

    // ---- provider api-keys -------------------------------------------------
    let (provider_keys_resealed, already_current, failed) = reseal_column(
        &mut tx,
        kp,
        current,
        "provider_key",
        "provider_key",
        "",
        "api_key_ciphertext",
        "api_key_nonce",
        "key_version",
        "",
    )
    .await?;
    report.provider_keys_resealed += provider_keys_resealed;
    report.already_current += already_current;
    report.failed.extend(failed);

    // ---- tenant certificate private keys ----------------------------------
    let (tenant_certs_resealed, already_current, failed) = reseal_column(
        &mut tx,
        kp,
        current,
        "tenant",
        "tenant",
        " cert",
        "cert_key_ciphertext",
        "cert_key_nonce",
        "cert_key_version",
        " WHERE cert_key_ciphertext IS NOT NULL AND cert_key_nonce IS NOT NULL \
         AND cert_key_version IS NOT NULL",
    )
    .await?;
    report.tenant_certs_resealed += tenant_certs_resealed;
    report.already_current += already_current;
    report.failed.extend(failed);

    // ---- limit_role.matching_key (decision D-16) --------------------------
    //
    // A third sealed column, added 2026-10-08. It has no `key_version` column of its own — the
    // version lives inside the envelope — so the "already current" test below reads it from there.
    // Two cases are handled, and they are different pieces of work:
    //   * a pre-D-16 PLAINTEXT value: sealing it is a MIGRATION, not a rotation, and this pass is
    //     the second of the two places that do it (the loader is the first, and the usual one);
    //   * a sealed value with an older version: the rotation this command exists for.
    let key_rows: Vec<(String, String)> = sqlx::query_as(&dialect::sql(
        "SELECT id, matching_key FROM limit_role WHERE matching_key IS NOT NULL ORDER BY id",
    ))
    .fetch_all(&mut *tx)
    .await?;
    for (id, value) in key_rows {
        let Some(sealed) = Sealed::from_text(&value) else {
            match kp.seal(value.as_bytes()) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE limit_role SET matching_key = ? WHERE id = ?",
                    ))
                    .bind(fresh.to_text())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.limit_keys_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("limit_role {id} matching_key: seal failed: {e}")),
            }
            continue;
        };
        if sealed.key_version == current {
            // Same rule as the two blocks above: the version tag is an independent knob from the key
            // material, so "already current" is VERIFIED by opening the value, not assumed from the
            // number. Counting an unopenable row as fine is how the operator ends up deleting the
            // only key that can read it.
            match kp.open(&sealed) {
                Ok(_) => report.already_current += 1,
                Err(e) => report.failed.push(format!(
                    "limit_role {id} matching_key: labelled key_version {current} (the CURRENT \
                     version) but it does NOT open under the current key — the key was most likely \
                     rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. Do NOT delete the \
                     previous key until this is resolved: {e}"
                )),
            }
            continue;
        }
        match kp.open(&sealed) {
            Ok(plaintext) => match kp.seal(&plaintext) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE limit_role SET matching_key = ? WHERE id = ?",
                    ))
                    .bind(fresh.to_text())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.limit_keys_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("limit_role {id} matching_key: seal failed: {e}")),
            },
            Err(e) => report.failed.push(format!(
                "limit_role {id} matching_key: cannot open key_version {}: {e}",
                sealed.key_version
            )),
        }
    }

    // One commit for both tables: a re-seal that failed halfway would leave the
    // rotation in a state where neither "before" nor "after" is true of the whole
    // DB, which is exactly what an operator cannot reason about.
    if report.provider_keys_resealed + report.tenant_certs_resealed + report.limit_keys_resealed > 0
    {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(report)
}
