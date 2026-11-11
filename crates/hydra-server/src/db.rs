//! Backend-selected sqlx pool, migrations, transaction primitives, error
//! classification, and the repo CRUD layer over runtime-checked queries.
//!
//! There is no compile-time SQL validation any more: the SQLite `.sqlx/` offline
//! cache was tied to one dialect, so every statement is built at runtime and
//! passed through [`dialect::sql`] for placeholder rewriting. PostgreSQL sees
//! `$1..$n`; MySQL sees `?`.
//!
//! Repo functions map the portable `BIGINT` columns onto `i64` in the row
//! structs and cast to the idiomatic `i32` / `bool` used by
//! [`hydra_core::model`] at the boundary. DB-native timestamps decode into
//! `chrono::NaiveDateTime` and are formatted back into the existing entity
//! string shape at that same boundary.

// ---------------------------------------------------------------------------
// WRITE PROTOCOL (Phase 2b-2)
// ---------------------------------------------------------------------------
//
// EVERY mutating entry point below uses `begin_write` (hydra_lock FOR UPDATE),
// bumps `config_revision` in that SAME transaction, and commits through
// `commit_write`/`bump_config_revision_on_conn`. Bump only when the statement
// actually changed a row (or an insert succeeded), so a no-op does not create a
// reload storm. Bulk writers bump ONCE per transaction.
//
// Audited mutators:
//   provider              insert/update/delete
//   provider_model        insert/update/delete
//   provider_key          insert/upsert/delete
//   tenant                insert/update/write_tenant/delete/update_tenant_cert
//   tenant_provider       insert/delete
//   tenant_model          insert/delete
//   limit_role            insert/update/delete (+ startup seal_legacy_limit_keys)
//   user_group            upsert/bulk/delete          (tenant-scoped)
//   hydra_user            upsert/bulk/delete          (tenant-scoped)
//   user_api_key          insert/bulk/delete          (tenant-scoped via parent)
//   tenant_access_token   insert/delete
//   global_role*          upsert/replace/bulk/delete
//   reseal_secrets        rotation
//   set_config_version    absolute revision write (lock + commit, no second bump)
//
// `backfill_legacy_certs` and `seal_legacy_limit_keys` are STARTUP-ONLY; the
// loader is read-only. New writes must be added to this checklist.

pub mod backend;
pub mod dialect;
#[doc(hidden)]
pub mod testing;

mod audit;
mod global_role;
mod identity;
mod limit_role;
mod pool;
mod provider;
mod reseal;
mod rows;
mod tenant;

pub use audit::{insert_admin_audit, AdminAudit};
pub use backend::{Db, DbConnection, Pool, Tx};

pub use global_role::{
    delete_global_role, list_global_roles, replace_global_role_providers, upsert_global_role,
    upsert_global_role_with_providers, upsert_global_roles_bulk,
};
pub use identity::{
    delete_tenant_access_token, delete_user, delete_user_api_key, delete_user_group,
    insert_tenant_access_token, insert_user_api_key, insert_user_api_keys_bulk,
    list_tenant_access_tokens, list_user_api_keys, list_user_groups, list_users, upsert_user,
    upsert_user_group, upsert_user_groups_bulk, upsert_users_bulk,
};
pub use limit_role::{
    delete_limit_role, get_limit_role, insert_limit_role, list_limit_roles, update_limit_role,
};
pub use pool::{
    begin_read_snapshot, begin_write, bump_config_revision, get_config_version, init_pool,
    is_retryable, is_unavailable, read_revision, run_migrate, set_config_version, ConfigVersion,
};
pub use provider::{
    delete_provider, delete_provider_key, delete_provider_model, get_provider, get_provider_key,
    get_provider_model, insert_provider, insert_provider_key, insert_provider_model,
    list_provider_keys, list_provider_keys_by_provider, list_provider_models, list_providers,
    update_provider, update_provider_model, upsert_provider_key,
};
pub use reseal::{backfill_legacy_certs, reseal_secrets, seal_legacy_limit_keys, ResealReport};
pub use tenant::{
    delete_tenant, delete_tenant_model, delete_tenant_provider, get_tenant, get_tenant_cert,
    get_tenant_model, get_tenant_provider, insert_tenant, insert_tenant_model,
    insert_tenant_provider, list_tenant_certs, list_tenant_models, list_tenant_providers,
    list_tenants, update_tenant, update_tenant_cert, write_tenant, TenantCert, TenantWrite,
};

#[allow(unused_imports)]
pub(crate) use global_role::list_global_roles_on;
pub(crate) use global_role::list_global_roles_on_conn;
pub(crate) use identity::{
    list_tenant_access_tokens_on, list_user_api_keys_on, list_user_groups_on, list_users_on,
};
pub(crate) use limit_role::list_limit_roles_on;
#[allow(unused_imports)]
pub(crate) use pool::bump_config_revision_on_conn;
pub(crate) use pool::read_revision_on;
#[cfg(test)]
pub(crate) use pool::{test_fault, test_pool};
pub(crate) use provider::{list_provider_keys_on, list_provider_models_on, list_providers_on};
pub(crate) use tenant::{
    list_tenant_certs_on, list_tenant_models_on, list_tenant_providers_on, list_tenants_on,
};
