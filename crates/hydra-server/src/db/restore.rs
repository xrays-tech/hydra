//! Replica rebuild: the config tables the control-plane snapshot rewrites, and
//! the single transaction that rebuilds them.
//!
//! Moved out of `db.rs` verbatim (plan T0.1, Phase 0) — no logic change. This is
//! the module the plan's T1/T6 edits target, so it is separated before those
//! land. `crypto_to_sqlx` stays in the parent and is reached as a child module
//! may reach an ancestor's private item.

use sqlx::SqlitePool;

use crate::crypto::KeyProvider;

use super::crypto_to_sqlx;

/// The config tables wiped on restore, in dependency order (children first,
/// parents last). The table name is a **static literal** per variant — never
/// assembled by `format!`, so there is no injection surface from a dynamic
/// table name.
#[derive(Clone, Copy, Debug)]
enum WipedTable {
    SubTenantRoute,
    SubTenant,
    ProviderKeyBinding,
    LimitRole,
    TenantModel,
    TenantProvider,
    Tenant,
    ProviderKey,
    ProviderModel,
    Provider,
}

impl WipedTable {
    /// The static `DELETE FROM …` statement for this variant. The whole
    /// statement is a compile-time literal — the table name is never
    /// interpolated at runtime, so there is no injection surface.
    ///
    /// Every variant is named: a future config table added to this enum must be
    /// given a statement, and the compiler says so. (The previous `_ =>
    /// unreachable!()` catch-all turned that compile error into a runtime panic
    /// on the replica-restore path — audit L-2.)
    fn delete_stmt(self) -> &'static str {
        match self {
            WipedTable::LimitRole => "DELETE FROM limit_role",
            WipedTable::TenantModel => "DELETE FROM tenant_model",
            WipedTable::TenantProvider => "DELETE FROM tenant_provider",
            WipedTable::Tenant => "DELETE FROM tenant",
            WipedTable::ProviderKey => "DELETE FROM provider_key",
            WipedTable::ProviderModel => "DELETE FROM provider_model",
            WipedTable::Provider => "DELETE FROM provider",
            WipedTable::SubTenantRoute => "DELETE FROM sub_tenant_route",
            WipedTable::SubTenant => "DELETE FROM sub_tenant",
            WipedTable::ProviderKeyBinding => "DELETE FROM provider_key_binding",
        }
    }
}

/// Rebuild every config table from `cfg` (+ the fidelity rows) in a single
/// transaction, AND write the `config_version` marker in that same transaction.
/// Secrets are re-sealed at this boundary with `kp`.
///
/// Table order: parents first on insert (FK), children first on delete.
///
/// **The marker belongs to the content** (review B4). It used to be written as
/// a separate statement after this function returned, so a crash (or any error)
/// in between left the replica holding one version's content and another
/// version's marker — contradicting the invariant this module documents, and,
/// since the election freshness gate compares the marker against the store, it
/// also made a node with perfectly correct content ineligible to lead.
pub async fn restore_config(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
    cfg: &hydra_core::config::ConfigData,
    fidelity: &crate::cluster::content::FidelityRows,
    version: u64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Wipe children before parents (static table names, no `format!`).
    //
    // `sub_tenant_route` is wiped FIRST: it references BOTH `sub_tenant` and
    // `provider`, so it must precede the deletion of either (FK is ON,
    // `db.rs`). `sub_tenant` references `tenant`, so it must precede the
    // `tenant` wipe below.
    for table in [
        WipedTable::SubTenantRoute,
        WipedTable::SubTenant,
        WipedTable::ProviderKeyBinding,
        WipedTable::LimitRole,
        WipedTable::TenantModel,
        WipedTable::TenantProvider,
        WipedTable::Tenant,
        WipedTable::ProviderKey,
        WipedTable::ProviderModel,
        WipedTable::Provider,
    ] {
        sqlx::query(table.delete_stmt()).execute(&mut *tx).await?;
    }

    // Providers.
    for p in cfg.providers.values() {
        sqlx::query(
            "INSERT INTO provider (id, key, name, endpoint, weight, created_at, updated_at, \
             max_concurrency, max_queue_depth, queue_wait_timeout_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&p.id)
        .bind(&p.key)
        .bind(&p.name)
        .bind(&p.endpoint)
        .bind(p.weight)
        .bind(&p.created_at)
        .bind(&p.updated_at)
        .bind(p.max_concurrency.map(|v| v as i64))
        .bind(p.max_queue_depth.map(|v| v as i64))
        .bind(p.queue_wait_timeout_ms.map(|v| v as i64))
        .execute(&mut *tx)
        .await?;
    }

    // Provider models (fidelity rows — includes offline models).
    for m in &fidelity.provider_models {
        sqlx::query(
            "INSERT INTO provider_model (id, key, name, provider_id, status) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&m.id)
        .bind(&m.key)
        .bind(&m.name)
        .bind(&m.provider_id)
        .bind(m.status)
        .execute(&mut *tx)
        .await?;
    }

    // Provider api-keys — from `fidelity.provider_keys`, which carries each
    // row's IDENTITY (`id`, `created_at`). Two defects lived here (audit G3):
    // the id/created_at were regenerated per materialization (so the replica was
    // not byte-faithful and `DELETE /provider-keys/{id}` 404'd after failover),
    // and the source map `cfg.provider_keys` is CLEARED on the wire — a replica
    // rebuilding from it would insert no keys at all.
    for k in &fidelity.provider_keys {
        let sealed = kp.seal(k.api_key.as_bytes()).map_err(crypto_to_sqlx)?;
        let nonce: &[u8] = &sealed.nonce;
        sqlx::query(
            "INSERT INTO provider_key (id, provider_id, api_key_ciphertext, api_key_nonce, \
             key_version, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&k.id)
        .bind(&k.provider_id)
        .bind(sealed.ciphertext)
        .bind(nonce)
        .bind(sealed.key_version as i64)
        .bind(&k.created_at)
        .execute(&mut *tx)
        .await?;
    }

    // Tenants (+ cert content columns + the access-token HASH).
    //
    // The hash is part of the row, not a follow-up step: `ConfigData` has no
    // field for it, so it rides the wire as a sealed fidelity row and is written
    // HERE (a separate UPDATE could silently affect zero rows if the tenant set
    // ever diverged, and a promoted replica would then answer 401 to every
    // request until an operator re-entered the token — §7-7). It arrives as a
    // hash and is reproduced VERBATIM: nothing is re-hashed at this boundary.
    let token_hashes: std::collections::HashMap<&str, &str> = fidelity
        .tenant_token_hashes
        .iter()
        .map(|(tenant_id, hash)| (tenant_id.as_str(), hash.as_str()))
        .collect();
    for t in cfg.tenants_by_domain.values() {
        sqlx::query(
            "INSERT INTO tenant (id, name, domain, auth_url, cert_key, cert_file, enabled, \
             created_at, updated_at, access_token_hash) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&t.id)
        .bind(&t.name)
        .bind(&t.domain)
        .bind(&t.auth_url)
        .bind(&t.cert_key)
        .bind(&t.cert_file)
        .bind(t.enabled)
        .bind(&t.created_at)
        .bind(&t.updated_at)
        .bind(token_hashes.get(t.id.as_str()).copied())
        .execute(&mut *tx)
        .await?;
        // Cert content (migration 0007): sealed key, plaintext cert PEM.
        let cert_meta = cfg.certs.get(&t.domain.to_lowercase());
        if let Some(cm) = cert_meta {
            if let Some(cert_pem) = &cm.cert_pem {
                let (ct, nonce, version) = match &cm.cert_key_pem {
                    Some(pem) => {
                        let sealed = kp.seal(pem.as_bytes()).map_err(crypto_to_sqlx)?;
                        (
                            Some(sealed.ciphertext),
                            Some(sealed.nonce.to_vec()),
                            Some(sealed.key_version as i64),
                        )
                    }
                    None => (None, None, None),
                };
                sqlx::query(
                    "UPDATE tenant SET cert_pem = ?, cert_key_ciphertext = ?, \
                     cert_key_nonce = ?, cert_key_version = ? WHERE id = ?",
                )
                .bind(cert_pem)
                .bind(ct)
                .bind(nonce)
                .bind(version)
                .bind(&t.id)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    // Tenant provider / model grants (fidelity rows — ids preserved).
    for tp in &fidelity.tenant_providers {
        sqlx::query("INSERT INTO tenant_provider (id, tenant_id, provider_id) VALUES (?, ?, ?)")
            .bind(&tp.id)
            .bind(&tp.tenant_id)
            .bind(&tp.provider_id)
            .execute(&mut *tx)
            .await?;
    }
    for tm in &fidelity.tenant_models {
        sqlx::query("INSERT INTO tenant_model (id, tenant_id, model_key) VALUES (?, ?, ?)")
            .bind(&tm.id)
            .bind(&tm.tenant_id)
            .bind(&tm.model_key)
            .execute(&mut *tx)
            .await?;
    }

    // Limit roles — from `fidelity`, NOT from the runtime config: that view
    // holds only the ENABLED rows the hot path matches on, so rebuilding from it
    // would delete every disabled role on the replica (audit G1).
    for r in &fidelity.limit_roles {
        sqlx::query(
            "INSERT INTO limit_role (id, name, matching_key, matching_model, matching_tenant, \
             matching_provider, limit_count, limit_token, window, enabled, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.id)
        .bind(&r.name)
        .bind(&r.matching_key)
        .bind(&r.matching_model)
        .bind(&r.matching_tenant)
        .bind(&r.matching_provider)
        .bind(r.limit_count)
        .bind(r.limit_token)
        .bind(&r.window)
        .bind(r.enabled)
        .bind(&r.created_at)
        .execute(&mut *tx)
        .await?;
    }

    // api-key prefix bindings — from `fidelity` for the same reason (G1); the
    // runtime config carries only the enabled ones.
    for b in &fidelity.key_prefix_bindings {
        sqlx::query(
            "INSERT INTO provider_key_binding (id, key_prefix, provider_id, enabled, created_at, \
              updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.id)
        .bind(&b.key_prefix)
        .bind(&b.provider_id)
        .bind(b.enabled)
        .bind(&b.created_at)
        .bind(&b.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    // Sub-tenants — from `fidelity` (FULL rows, disabled included): the runtime
    // `cfg.sub_tenants` keeps only the enabled ones (G1, like the bindings above).
    // `tenant` (the FK) is already inserted above.
    for s in &fidelity.sub_tenants {
        sqlx::query(
            "INSERT INTO sub_tenant (id, tenant_id, name, key_prefix, enabled, created_at, \
              updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(&s.tenant_id)
        .bind(&s.name)
        .bind(&s.key_prefix)
        .bind(s.enabled)
        .bind(&s.created_at)
        .bind(&s.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    // Sub-tenant routes — from `fidelity` (FULL rows). Each row pins a
    // sub-tenant's traffic to one provider, optionally scoped to a model
    // (`model_key = NULL` is the default route). Both FKs — `sub_tenant` and
    // `provider` — are already inserted above.
    for r in &fidelity.sub_tenant_routes {
        sqlx::query(
            "INSERT INTO sub_tenant_route (id, sub_tenant_id, model_key, provider_id, enabled, \
              created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.id)
        .bind(&r.sub_tenant_id)
        .bind(&r.model_key)
        .bind(&r.provider_id)
        .bind(r.enabled)
        .bind(&r.created_at)
        .bind(&r.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    // The version marker commits with the content it describes, or neither does.
    sqlx::query(
        "INSERT INTO config_meta (key, value) VALUES ('config_version', ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(version.to_string())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}
