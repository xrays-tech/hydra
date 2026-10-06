//! # Splitting `ConfigData` into per-entity values (ADR-0001, plan T3.1 I/O half)
//!
//! [`crate::cluster::arachne_store`] publishes a **tree**: one value per entity,
//! keyed by the entity's path, named by the hash of a table of contents. The key
//! layout has existed since T2.1 and the commit point since T2.2, but until now
//! nothing produced a tree from the runtime config — the wire format sealed the
//! whole config as ONE payload (`SnapshotWire`), so a tree would have been a
//! single value wearing a tree's clothes.
//!
//! This module is that missing producer and its inverse. It is deliberately a
//! pure function pair: no cluster, no database, no key provider. The secret
//! handling stays where it already lives — `ConfigData` in memory holds plaintext
//! provider keys and `CertMeta::cert_key_pem` **is not serialised at all**
//! (`skip_serializing`, by design: it is re-derived at the DB boundary), so a tree
//! built here contains no private key material. What the tree does NOT carry is
//! the sealed secret blob; a caller that needs to reproduce the SQLite replica
//! exactly still has to go through the wire's sealing path, and that is called out
//! in the tests rather than hidden.
//!
//! ## Why one value per entity, again
//!
//! Because the alternative is a config that rewrites itself. A whole-config
//! payload has one hash, so adding a single tenant changes it, so EVERY node
//! re-publishes and re-reads the entire config — and the log grows by the size of
//! the config rather than the size of the change. Per-entity values make the
//! change proportional to what changed, which is also what makes the toc's
//! per-entity content hashes worth carrying.

use std::collections::HashSet;

use hydra_core::config::ConfigData;
use hydra_core::model::{LimitRole, ProviderKeyBinding, SubTenant, SubTenantRoute};

use super::arachne_keys::{
    content_hash, validate_entities, EntityPath, KeysError, Toc, TocEntry, TOC_FORMAT,
};
use super::arachne_store::ConfigTree;

/// Why a config could not be split or rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityCodecError {
    /// An entity id cannot be carried by a key.
    Keys(KeysError),
    /// An entity could not be encoded, or a stored entity could not be decoded.
    Serde { path: String, reason: String },
    /// The builder was handed a value it cannot place in a tree.
    Unsupported { reason: String },
}

impl std::fmt::Display for EntityCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keys(e) => write!(f, "config key space: {e}"),
            Self::Serde { path, reason } => {
                write!(f, "entity {path} could not be decoded: {reason}")
            }
            Self::Unsupported { reason } => write!(f, "cannot build a config tree: {reason}"),
        }
    }
}

impl std::error::Error for EntityCodecError {}

impl From<KeysError> for EntityCodecError {
    fn from(e: KeysError) -> Self {
        Self::Keys(e)
    }
}

/// One entity's bytes, tagged with the path they belong to.
///
/// Returned as a list rather than a map so a caller can see the ORDER the
/// publisher used — which matters, because the toc's bytes are the tree's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityBlob {
    /// Where this entity lives in the tree.
    pub path: EntityPath,
    /// Its JSON encoding.
    pub bytes: Vec<u8>,
}

fn encode<T: serde::Serialize>(
    path: &EntityPath,
    value: &T,
) -> Result<EntityBlob, EntityCodecError> {
    let bytes = serde_json::to_vec(value).map_err(|e| EntityCodecError::Serde {
        path: path.to_key_segment(),
        reason: e.to_string(),
    })?;
    Ok(EntityBlob {
        path: path.clone(),
        bytes,
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    path: &EntityPath,
    bytes: &[u8],
) -> Result<T, EntityCodecError> {
    serde_json::from_slice(bytes).map_err(|e| EntityCodecError::Serde {
        path: path.to_key_segment(),
        reason: e.to_string(),
    })
}

/// Split `cfg` into one blob per entity.
///
/// The output is **sorted by path** (so the encoding is deterministic and two
/// publishers of the same config produce the same tree name).
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed or an entity cannot be
/// encoded. Both are refused rather than skipped: a config tree that silently
/// dropped an entity would serve a config nobody configured.
pub fn split_config(cfg: &ConfigData) -> Result<Vec<EntityBlob>, EntityCodecError> {
    let mut out: Vec<EntityBlob> = Vec::new();

    // Every id is checked HERE, when the blob is produced. The check also runs
    // later, inside `toc_of`, but by then the caller has already built a tree from
    // unvalidated paths — and the failure mode that matters (an id containing `/`
    // addressing another entity's namespace) has to be refused before anything is
    // constructed, not before it is published.
    let check = |path: &EntityPath| -> Result<(), EntityCodecError> {
        validate_entities(&[TocEntry {
            path: path.clone(),
            content_hash: [0u8; 32],
            len: 0,
        }])?;
        Ok(())
    };

    for (domain, tenant) in &cfg.tenants_by_domain {
        // Keyed by the tenant's immutable id, not by the domain it is reachable
        // at: the domain is a route and can be re-pointed, while the id is what
        // every other entity refers to.
        let path = EntityPath::Tenant(tenant.id.clone());
        check(&path)?;
        out.push(encode(&path, tenant)?);
        let _ = domain;
    }
    for (key, providers) in &cfg.models_by_key {
        let path = EntityPath::Model(key.clone());
        check(&path)?;
        out.push(encode(&path, providers)?);
    }
    for (id, provider) in &cfg.providers {
        let path = EntityPath::Provider(id.clone());
        check(&path)?;
        out.push(encode(&path, provider)?);
    }
    for (id, keys) in &cfg.provider_keys {
        let path = EntityPath::ProviderKey(id.clone());
        check(&path)?;
        out.push(encode(&path, keys)?);
    }
    for (tenant_id, providers) in &cfg.tenant_providers {
        let path = EntityPath::TenantProvider(tenant_id.clone());
        check(&path)?;
        out.push(encode(&path, providers)?);
    }
    for (tenant_id, models) in &cfg.tenant_models {
        let path = EntityPath::TenantModel(tenant_id.clone());
        check(&path)?;
        out.push(encode(&path, models)?);
    }
    for role in &cfg.limit_roles {
        let path = EntityPath::LimitRole(role.id.clone());
        check(&path)?;
        out.push(encode(&path, role)?);
    }
    for binding in &cfg.key_prefix_bindings {
        let path = EntityPath::KeyBinding(binding.id.clone());
        check(&path)?;
        out.push(encode(&path, binding)?);
    }
    for sub in &cfg.sub_tenants {
        let path = EntityPath::SubTenant(sub.id.clone());
        check(&path)?;
        out.push(encode(&path, sub)?);
    }
    for route in &cfg.sub_tenant_routes {
        let path = EntityPath::SubTenantRoute(route.id.clone());
        check(&path)?;
        out.push(encode(&path, route)?);
    }
    for (domain, cert) in &cfg.certs {
        let path = EntityPath::Cert(domain.clone());
        check(&path)?;
        out.push(encode(&path, cert)?);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The tree these blobs form, ready for [`crate::cluster::arachne_store`].
///
/// # Errors
/// [`EntityCodecError::Unsupported`] when two entities claim the same path, which
/// would make the tree's contents depend on insertion order.
pub fn tree_of(blobs: &[EntityBlob]) -> Result<ConfigTree, EntityCodecError> {
    let mut tree = ConfigTree::new();
    for blob in blobs {
        if tree.insert(blob.path.clone(), blob.bytes.clone()).is_some() {
            return Err(EntityCodecError::Unsupported {
                reason: format!(
                    "two entities claim the path {}; a tree cannot name both",
                    blob.path.to_key_segment()
                ),
            });
        }
    }
    Ok(tree)
}

/// The toc describing `tree`, with one entry per entity.
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed.
pub fn toc_of(tree: &ConfigTree) -> Result<Toc, EntityCodecError> {
    let entries: Vec<TocEntry> = tree
        .iter()
        .map(|(path, bytes)| TocEntry {
            path: path.clone(),
            content_hash: content_hash(bytes),
            len: bytes.len() as u32,
        })
        .collect();
    Ok(Toc::new(TOC_FORMAT, entries)?)
}

/// Rebuild a `ConfigData` from a tree.
///
/// The derived index `tenants_by_id` is **rebuilt here**, because `serde(skip)`
/// on that field means the tree cannot carry it — and a missing index is not a
/// cosmetic problem: the tenant-token gate resolves a token to a tenant through
/// it, so an empty index turns every valid token into a 403.
///
/// # Errors
/// [`EntityCodecError`] when an entity cannot be decoded or a path is not one
/// this build knows. Unknown paths are refused rather than ignored: they mean
/// this node is reading a tree written by a different version.
pub fn build_config(tree: &ConfigTree) -> Result<ConfigData, EntityCodecError> {
    let mut cfg = ConfigData::default();

    for (path, bytes) in tree {
        match path {
            EntityPath::Tenant(_) => {
                let tenant = decode::<hydra_core::model::Tenant>(path, bytes)?;
                cfg.tenants_by_domain.insert(tenant.domain.clone(), tenant);
            }
            EntityPath::Model(key) => {
                let providers = decode(path, bytes)?;
                cfg.models_by_key.insert(key.clone(), providers);
            }
            EntityPath::Provider(id) => {
                let provider = decode(path, bytes)?;
                cfg.providers.insert(id.clone(), provider);
            }
            EntityPath::ProviderKey(id) => {
                let keys = decode(path, bytes)?;
                cfg.provider_keys.insert(id.clone(), keys);
            }
            EntityPath::TenantProvider(id) => {
                let providers: HashSet<String> = decode(path, bytes)?;
                cfg.tenant_providers.insert(id.clone(), providers);
            }
            EntityPath::TenantModel(id) => {
                let models: HashSet<String> = decode(path, bytes)?;
                cfg.tenant_models.insert(id.clone(), models);
            }
            EntityPath::LimitRole(_) => {
                let role = decode::<LimitRole>(path, bytes)?;
                cfg.limit_roles.push(role);
            }
            EntityPath::KeyBinding(_) => {
                let binding = decode::<ProviderKeyBinding>(path, bytes)?;
                cfg.key_prefix_bindings.push(binding);
            }
            EntityPath::SubTenant(_) => {
                let sub = decode::<SubTenant>(path, bytes)?;
                cfg.sub_tenants.push(sub);
            }
            EntityPath::SubTenantRoute(_) => {
                let route = decode::<SubTenantRoute>(path, bytes)?;
                cfg.sub_tenant_routes.push(route);
            }
            EntityPath::Cert(domain) => {
                let cert = decode(path, bytes)?;
                cfg.certs.insert(domain.clone(), cert);
            }
            // Not part of the config tree (yet). Refused rather than skipped: a
            // skipped path means this node is reading a tree written by another
            // version and would serve a config silently missing that entity.
            other => {
                return Err(EntityCodecError::Unsupported {
                    reason: format!(
                        "entity {} is not part of the config tree this build knows",
                        other.to_key_segment()
                    ),
                })
            }
        }
    }

    // Order the vectors so two nodes that materialized the same tree hold the
    // same value, not merely an equivalent one: `SubTenantRoute` order decides
    // which of two overlapping prefixes wins, so "equivalent" is not enough.
    cfg.limit_roles.sort_by(|a, b| a.id.cmp(&b.id));
    cfg.key_prefix_bindings.sort_by(|a, b| a.id.cmp(&b.id));
    cfg.sub_tenants.sort_by(|a, b| a.id.cmp(&b.id));
    cfg.sub_tenant_routes.sort_by(|a, b| a.id.cmp(&b.id));

    cfg.reindex_tenants();
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::{CertMeta, ModelProvider};
    use hydra_core::model::{Provider, Tenant};

    fn tenant(id: &str, domain: &str) -> Tenant {
        Tenant {
            id: id.to_string(),
            name: format!("name-{id}"),
            domain: domain.to_string(),
            auth_url: format!("https://auth.{domain}"),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            key: format!("key-{id}"),
            name: format!("provider-{id}"),
            endpoint: format!("https://{id}.example"),
            weight: 1,
            created_at: String::new(),
            updated_at: String::new(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        }
    }

    fn config() -> ConfigData {
        let mut cfg = ConfigData::default();
        cfg.tenants_by_domain
            .insert("acme.example".into(), tenant("t1", "acme.example"));
        cfg.tenants_by_domain
            .insert("globex.example".into(), tenant("t2", "globex.example"));
        cfg.providers.insert("p1".into(), provider("p1"));
        cfg.provider_keys
            .insert("p1".into(), vec!["sk-one".into(), "sk-two".into()]);
        cfg.models_by_key.insert(
            "gpt-x".into(),
            vec![ModelProvider {
                provider_id: "p1".into(),
                weight: 1,
            }],
        );
        cfg.tenant_models
            .insert("t1".into(), HashSet::from(["gpt-x".to_string()]));
        cfg.certs.insert(
            "acme.example".into(),
            CertMeta {
                domain: "acme.example".into(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some("-----BEGIN CERTIFICATE-----".into()),
                // Deliberately set: it must NOT survive the round trip (the
                // field is `skip_serializing`), and the round-trip test below
                // asserts that rather than assuming it.
                cert_key_pem: Some("-----BEGIN PRIVATE KEY-----".into()),
            },
        );
        cfg.reindex_tenants();
        cfg
    }

    /// The pair is a round trip, and it produces the SAME tree name twice — the
    /// property that keeps one publish from looking like a new tree to every
    /// other node.
    ///
    /// Falsification: stop sorting in `split_config` and the two names diverge
    /// whenever the hash maps iterate differently.
    #[test]
    fn splitting_and_rebuilding_round_trips_deterministically() {
        let cfg = config();
        let blobs = split_config(&cfg).expect("split");
        let tree = tree_of(&blobs).expect("tree");

        let again = tree_of(&split_config(&cfg).expect("split again")).expect("tree again");
        assert_eq!(
            toc_of(&tree).expect("toc").hash(),
            toc_of(&again).expect("toc").hash(),
            "the same config must produce the same tree name every time"
        );

        let rebuilt = build_config(&tree).expect("rebuild");

        // `CertMeta::cert_key_pem` is `skip_serializing` on purpose: plaintext
        // private keys must never appear in any serialized form (that is an
        // existing, documented invariant of `ConfigData`, not something this
        // codec introduces). So the round trip is compared against the original
        // with that one field cleared — and the clearing is ASSERTED, not
        // assumed, by the dedicated test below.
        let mut expected = cfg.clone();
        for cert in expected.certs.values_mut() {
            cert.cert_key_pem = None;
        }
        assert_eq!(
            rebuilt, expected,
            "the rebuilt config must equal the original, not merely resemble it"
        );
    }

    /// The one field that does not survive, asserted rather than assumed — and
    /// asserted in the direction that matters: it must be ABSENT afterwards.
    ///
    /// Falsification: remove `skip_serializing` from `cert_key_pem` (or route the
    /// cert entity through a format that ignores it) and this test fails, which is
    /// the point: a private key leaking into a replicated config tree is exactly
    /// the kind of "helpful" regression a round-trip test would otherwise wave
    /// through.
    #[test]
    fn plaintext_private_key_material_never_enters_the_tree() {
        let cfg = config();
        let blobs = split_config(&cfg).expect("split");

        // Nothing in the tree's bytes may contain the private key PEM.
        for blob in &blobs {
            let text = String::from_utf8_lossy(&blob.bytes);
            assert!(
                !text.contains("PRIVATE KEY"),
                "entity {} carries private key material: {text}",
                blob.path.to_key_segment()
            );
        }

        let tree = tree_of(&blobs).expect("tree");
        let rebuilt = build_config(&tree).expect("rebuild");
        for (domain, cert) in &rebuilt.certs {
            assert!(
                cert.cert_key_pem.is_none(),
                "cert {domain} must come back without its private key; the sealing path is what \
                 re-supplies it, and this codec must not be a second one"
            );
            assert!(
                cert.cert_pem.is_some(),
                "the PUBLIC certificate must survive: dropping it would break TLS"
            );
        }
    }

    /// The derived index is rebuilt, and that is not cosmetic: an empty
    /// `tenants_by_id` turns every valid tenant token into a 403.
    ///
    /// Falsification: drop the `reindex_tenants()` call and this fails while the
    /// round trip above still passes — which is why this assertion is separate.
    #[test]
    fn the_derived_tenant_index_is_rebuilt() {
        let cfg = config();
        let tree = tree_of(&split_config(&cfg).expect("split")).expect("tree");
        let rebuilt = build_config(&tree).expect("rebuild");

        assert_eq!(rebuilt.tenants_by_id.len(), 2);
        assert!(rebuilt.tenants_by_id.contains_key("t1"));
        assert!(rebuilt.tenants_by_id.contains_key("t2"));
        assert_eq!(rebuilt.tenants_by_id["t1"].domain, "acme.example");
    }

    /// The point of the split: changing ONE entity changes ONE value.
    ///
    /// Falsification: encode the whole config per entity and the "every other
    /// value is byte-identical" assertion fails.
    #[test]
    fn changing_one_entity_changes_exactly_one_value() {
        let before = config();
        let mut after = before.clone();
        let mut changed = tenant("t2", "globex.example");
        changed.name = "a different name".to_string();
        after
            .tenants_by_domain
            .insert("globex.example".into(), changed);

        let tree_before = tree_of(&split_config(&before).expect("split")).expect("tree");
        let tree_after = tree_of(&split_config(&after).expect("split")).expect("tree");

        let mut differing = Vec::new();
        for (path, bytes) in &tree_before {
            match tree_after.get(path) {
                Some(other) if other == bytes => {}
                _ => differing.push(path.to_key_segment()),
            }
        }
        assert_eq!(
            differing,
            vec!["tenant/t2".to_string()],
            "editing tenant t2 must change only tenant/t2"
        );
        assert_eq!(tree_before.len(), tree_after.len());
        assert_ne!(
            toc_of(&tree_before).expect("toc").hash(),
            toc_of(&tree_after).expect("toc").hash(),
            "the tree name must change even though only one value did"
        );
    }

    /// Removing an entity removes its value from the tree, and the rebuilt config
    /// no longer has it — "deletion is absence from the toc", not a tombstone.
    #[test]
    fn a_removed_entity_is_absent_from_the_tree() {
        let mut cfg = config();
        cfg.providers.remove("p1");
        cfg.provider_keys.remove("p1");

        let tree = tree_of(&split_config(&cfg).expect("split")).expect("tree");
        assert!(
            !tree.contains_key(&EntityPath::Provider("p1".into())),
            "a removed provider must not appear in the tree"
        );
        let rebuilt = build_config(&tree).expect("rebuild");
        assert!(rebuilt.providers.is_empty());
    }

    /// An id that cannot be keyed is refused when the config is SPLIT, before
    /// anything reaches the cluster.
    ///
    /// Falsification: skip the id check and a tenant whose id contains `/` would
    /// be written into another entity's namespace.
    #[test]
    fn an_unkeyable_id_is_refused_at_split_time() {
        let mut cfg = ConfigData::default();
        cfg.tenants_by_domain.insert(
            "evil.example".into(),
            tenant("../../ctl/head", "evil.example"),
        );
        let got = split_config(&cfg);
        assert!(
            matches!(
                got,
                Err(EntityCodecError::Keys(KeysError::IdHasSeparator { .. }))
            ),
            "a path-bearing tenant id must be refused, got {got:?}"
        );
    }

    /// Two entities claiming one path would make the tree depend on insertion
    /// order, so it is refused rather than silently resolved.
    #[test]
    fn two_entities_claiming_one_path_are_refused() {
        let blobs = vec![
            EntityBlob {
                path: EntityPath::Provider("p1".into()),
                bytes: b"first".to_vec(),
            },
            EntityBlob {
                path: EntityPath::Provider("p1".into()),
                bytes: b"second".to_vec(),
            },
        ];
        let got = tree_of(&blobs);
        assert!(
            matches!(got, Err(EntityCodecError::Unsupported { .. })),
            "a duplicated path must be refused, got {got:?}"
        );
    }

    /// A path from a future version must be refused, not ignored: ignoring it
    /// would serve a config that is silently missing whatever that entity was.
    ///
    /// Falsification: make the match's catch-all skip unknowns and this passes
    /// with a config missing an entity.
    #[test]
    fn an_unknown_entity_path_is_refused_rather_than_ignored() {
        // `Meta` is a valid path in the key space but is not part of the config
        // tree, so it exercises exactly this case.
        let mut tree = ConfigTree::new();
        tree.insert(EntityPath::Meta, b"{}".to_vec());
        let got = build_config(&tree);
        assert!(
            matches!(got, Err(EntityCodecError::Unsupported { .. })),
            "an entity this build cannot place must be refused, got {got:?}"
        );
    }
}
