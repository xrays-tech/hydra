//! # Standby replica materialization (cluster P2)
//!
//! A leader-candidate node that does NOT hold the lease (standby) keeps its
//! local SQLite in sync with the active leader by materializing every applied
//! control snapshot into it ([`materialize`]). On promotion the replica IS
//! the config DB — the version continues from `config_meta`, never resets.
//!
//! The rebuild is a single transaction (see `db::restore_config`), so a crash
//! mid-restore never leaves a half-config; the previous (last-good) replica
//! remains in place until the new one commits.

use sqlx::SqlitePool;

use crate::cluster::snapshot::{SnapshotError, SnapshotWire};
use crate::crypto::KeyProvider;
use crate::db;

/// Materialize a received control snapshot into the local replica DB:
/// hydrate (decrypt secrets) → full-table rebuild → persist the version.
pub async fn materialize(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
    wire: &SnapshotWire,
) -> Result<(), SnapshotError> {
    let cfg = wire.clone().hydrate(kp)?;
    db::restore_config(
        pool,
        kp,
        &cfg,
        &wire.provider_models,
        &wire.tenant_providers,
        &wire.tenant_models,
    )
    .await?;
    db::set_config_version(pool, wire.version).await?;
    Ok(())
}

/// The last-applied config version stored in the replica (`None` = fresh DB).
pub async fn replica_version(pool: &SqlitePool) -> Result<Option<u64>, sqlx::Error> {
    db::get_config_version(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_helpers_roundtrip() {
        // Sync smoke of config_meta get/set against a real in-memory DB.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = crate::db::init_pool("sqlite::memory:")
                .await
                .expect("init_pool");
            crate::db::run_migrate(&pool).await.expect("migrate");
            assert_eq!(replica_version(&pool).await.unwrap(), None);
            crate::db::set_config_version(&pool, 7).await.expect("set");
            assert_eq!(replica_version(&pool).await.unwrap(), Some(7));
            crate::db::set_config_version(&pool, 8)
                .await
                .expect("update");
            assert_eq!(replica_version(&pool).await.unwrap(), Some(8));
        });
    }
}
