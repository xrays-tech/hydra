//! T10.1 — dependency firewall: `cargo tree -p hydra-core` contains NONE of
//! `tokio` / `pingora` / `sqlx` / `reqwest` / `hyper`.
//!
//! T10.2 — every public struct/enum is `Send + Sync` (compile-time).
//!
//! The authoritative CI gate is the `cargo tree` grep step in
//! `.github/workflows/ci.yml`; this test mirrors it locally so
//! `cargo test -p hydra-core` fails fast if a forbidden dep leaks in.

use hydra_core::breaker::BreakerView;
use hydra_core::config::{CertMeta, ConcurrencyPolicy, ConfigData, ModelProvider};
use hydra_core::limit::MatchCtx;
use hydra_core::model::{
    Candidate, LimitRole, Provider, ProviderKey, ProviderKind, ProviderModel, RouteError, Tenant,
    TenantModel, TenantProvider, Usage, UsageRecord,
};
use hydra_core::rewrite::EndpointUrl;
use hydra_core::swrr::SwrrState;

const FORBIDDEN: &[&str] = &["tokio", "pingora", "sqlx", "reqwest", "hyper"];

/// `cargo tree` prints each crate as `name vX.Y.Z`; matching `"<name> v"` is
/// precise (e.g. `sqlx-core v..` does not contain `"sqlx v"`).
#[test]
fn cargo_tree_core_has_no_io() {
    let output = std::process::Command::new(env!("CARGO"))
        .args(["tree", "-p", "hydra-core", "--no-default-features"])
        .output()
        .expect("failed to invoke `cargo tree`; is cargo on PATH?");

    assert!(
        output.status.success(),
        "`cargo tree -p hydra-core` failed.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let offenders: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|name| {
            stdout.contains(&format!(" {name} v"))
                || stdout.contains(&format!("\n{name} v"))
                || stdout.starts_with(&format!("{name} v"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "hydra-core pulls forbidden I/O dependencies: {offenders:?}\n\
         full `cargo tree` output:\n{stdout}"
    );
}

/// Compile-time assertion that all public types are `Send + Sync` (T10.2).
/// If any type loses `Send + Sync`, this test fails to compile.
#[test]
fn public_types_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Provider>();
    assert_send_sync::<ProviderModel>();
    assert_send_sync::<ProviderKey>();
    assert_send_sync::<Tenant>();
    assert_send_sync::<TenantProvider>();
    assert_send_sync::<TenantModel>();
    assert_send_sync::<LimitRole>();
    assert_send_sync::<Candidate>();
    assert_send_sync::<RouteError>();
    assert_send_sync::<ProviderKind>();
    assert_send_sync::<Usage>();
    assert_send_sync::<UsageRecord>();

    assert_send_sync::<ConfigData>();
    assert_send_sync::<ModelProvider>();
    assert_send_sync::<ConcurrencyPolicy>();
    assert_send_sync::<CertMeta>();

    assert_send_sync::<SwrrState>();
    assert_send_sync::<EndpointUrl>();
    assert_send_sync::<MatchCtx<'static>>();

    // The breaker trait object must be usable across threads (the server holds
    // it behind `Arc<CircuitBreaker>`).
    assert_send_sync::<Box<dyn BreakerView>>();
}
