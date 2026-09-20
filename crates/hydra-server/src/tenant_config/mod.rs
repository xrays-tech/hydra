//! # Tenant config write forwarding (sub-tenant v2, decision A-2)
//!
//! The data plane's trust-scoped path to the leader's internal control plane
//! for tenant **config writes** (sub-tenant self-service CRUD). See
//! [`forward::TenantConfigForwarder`].
//!
//! The module exists so the data plane can reach the leader WITHOUT holding the
//! `NodeRegistry` (A-2 precond. 8): it carries only a shared cluster token (node
//! identity) and a single leader-URL closure — exactly "ask where the writer
//! is" and nothing more.
//!
//! Gated on `proxy`: it is wired into the data-plane [`crate::proxy::AppState`].

pub mod forward;

pub use forward::{TenantConfigForwardError, TenantConfigForwarder};
