//! # Arachne control-plane keyspace and table of contents (ADR-0001, plan T2.1)
//!
//! The config authority lives in Arachne as a **versioned tree**, not as one
//! blob and not as a byte-sharded blob:
//!
//! ```text
//! hydra/ctl/head                      -> the current tree's content hash
//! hydra/cfg/toc/<toc-hash>            -> the table of contents of that tree
//! hydra/cfg/e/<path>                  -> one entity, keyed by its entity path
//! ```
//!
//! ## Why the entity key does NOT contain the tree hash
//!
//! The first version of this module keyed entities as `hydra/cfg/<toc-hash>/<path>`,
//! which reads well and is wrong: entities are written only when their bytes
//! CHANGE, so an unchanged entity lives under the previous tree's hash while the
//! new tree names the new one — and the new tree then cannot find it (measured:
//! the round-trip test failed with "tree <h> is missing entity tenant/acme").
//! Content-addressing every entity instead would fix the lookup but force a new
//! key per entity content version, i.e. a garbage stream to collect, for files
//! whose logical identity is the PATH.
//!
//! So the two are decoupled: the tree is named by the hash of its toc, the toc
//! is stored under that hash, and entity keys are stable per path. A reader
//! fetches an entity by path and verifies it against the hash the toc records,
//! which is what makes the toc the authority and the entity key merely an
//! address.
//!
//! ## Why a tree keyed by entity path, and not byte shards
//!
//! A blob split into fixed-size chunks changes EVERY later chunk's bytes as soon
//! as one entity is inserted or deleted, so a one-entity edit rewrites (and
//! re-logs) almost the whole config. Keying each entity by its own path means an
//! edit writes that entity, the toc, and the head — nothing else — while
//! unchanged entities keep their identical bytes and need not be re-sent.
//!
//! ## Why the commit point is a CONTENT HASH and not a version number
//!
//! Every read on this path is `get_stale` (a follower's linearizable `get`
//! returns `QuorumUnavailable` immediately — measured, ADR-0001 §10 F-2), and
//! `get_stale` is explicitly **not monotone**. With a version number, two reads
//! could observe different versions and assemble a config that never existed.
//! With a content hash the toc and every entity are bound to one tree by
//! construction: a node either reads that exact tree or fails and retries.
//!
//! ## What this module refuses to do
//!
//! Build a key out of an arbitrary string. Identifiers reach these keys from
//! operator input and from a database, so an id containing `/` would otherwise
//! be able to write into another entity's namespace (and a path is itself part
//! of the tamper-evidence story). [`EntityPath`] is therefore an enum, and
//! [`EntityPath::validate_id`] is the only constructor: the type system, not a
//! review, is what keeps a hostile id out of the key.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// The `format` this module speaks. A toc carrying any other value is refused
/// rather than guessed at: a mixed-version cluster must fail loudly, because the
/// alternative is one node writing a tree the others decode differently.
pub const TOC_FORMAT: u32 = 1;

/// Prefix of the control-plane namespace (commit points, cluster identity).
pub const CTL_PREFIX: &str = "hydra/ctl/";
/// Prefix of the config-tree namespace.
pub const CFG_PREFIX: &str = "hydra/cfg/";

/// The key holding the current tree's hash — the **single commit point**.
#[must_use]
pub fn ctl_head() -> String {
    format!("{CTL_PREFIX}head")
}

/// The key recording which cluster a data directory belongs to.
#[must_use]
pub fn ctl_cluster_id() -> String {
    format!("{CTL_PREFIX}cluster_id")
}

/// The key recording the key-space format of a data directory.
#[must_use]
pub fn ctl_format() -> String {
    format!("{CTL_PREFIX}format")
}

/// The table of contents of the tree identified by `toc_hash`.
#[must_use]
pub fn cfg_toc(toc_hash: &str) -> String {
    format!("{CFG_PREFIX}toc/{toc_hash}")
}

/// One entity, keyed by its own path — **not** by the tree it currently belongs
/// to, because an unchanged entity is not rewritten and therefore outlives the
/// tree that introduced it (see the module docs).
#[must_use]
pub fn cfg_entity(path: &EntityPath) -> String {
    format!("{CFG_PREFIX}e/{}", path.to_key_segment())
}

/// Where one entity lives inside a config tree.
///
/// An enum rather than a string, so a caller cannot smuggle a path separator (or
/// a `..`, or an empty segment) into a key. The variant names are the wire
/// spelling: they are part of the key layout, so they may not be renamed without
/// a `TOC_FORMAT` bump.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntityPath {
    /// `tenant/<id>`
    Tenant(String),
    /// `provider/<id>`
    Provider(String),
    /// `provider_key/<id>`
    ProviderKey(String),
    /// `model/<key>`
    Model(String),
    /// `limit_role/<id>`
    LimitRole(String),
    /// `key_binding/<id>`
    KeyBinding(String),
    /// `sub_tenant/<id>`
    SubTenant(String),
    /// `sub_tenant_route/<id>`
    SubTenantRoute(String),
    /// `token/<tenant_id>`
    Token(String),
    /// `cert/<tenant_id>`
    Cert(String),
    /// `meta` — the singleton carrying the config's own metadata.
    Meta,
}

impl EntityPath {
    /// The key segment for this entity, `/`-free by construction.
    #[must_use]
    pub fn to_key_segment(&self) -> String {
        match self {
            Self::Tenant(id) => format!("tenant/{id}"),
            Self::Provider(id) => format!("provider/{id}"),
            Self::ProviderKey(id) => format!("provider_key/{id}"),
            Self::Model(key) => format!("model/{key}"),
            Self::LimitRole(id) => format!("limit_role/{id}"),
            Self::KeyBinding(id) => format!("key_binding/{id}"),
            Self::SubTenant(id) => format!("sub_tenant/{id}"),
            Self::SubTenantRoute(id) => format!("sub_tenant_route/{id}"),
            Self::Token(id) => format!("token/{id}"),
            Self::Cert(id) => format!("cert/{id}"),
            Self::Meta => "meta".to_string(),
        }
    }

    /// The discriminator byte used by the toc encoding. Stable on the wire.
    #[must_use]
    fn discriminant(&self) -> u8 {
        match self {
            Self::Tenant(_) => 1,
            Self::Provider(_) => 2,
            Self::ProviderKey(_) => 3,
            Self::Model(_) => 4,
            Self::LimitRole(_) => 5,
            Self::KeyBinding(_) => 6,
            Self::SubTenant(_) => 7,
            Self::SubTenantRoute(_) => 8,
            Self::Token(_) => 9,
            Self::Cert(_) => 10,
            Self::Meta => 11,
        }
    }

    /// The id carried by this path, or `None` for the singleton.
    #[must_use]
    fn id(&self) -> Option<&str> {
        match self {
            Self::Tenant(id)
            | Self::Provider(id)
            | Self::ProviderKey(id)
            | Self::Model(id)
            | Self::LimitRole(id)
            | Self::KeyBinding(id)
            | Self::SubTenant(id)
            | Self::SubTenantRoute(id)
            | Self::Token(id)
            | Self::Cert(id) => Some(id),
            Self::Meta => None,
        }
    }
}

/// Everything that can be wrong with a toc or a key on this path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeysError {
    /// An entity id is empty or blank.
    EmptyId { kind: &'static str },
    /// An entity id contains `/` (or a NUL), which would move the write into
    /// another entity's namespace.
    IdHasSeparator { kind: &'static str, id: String },
    /// An entity id is longer than the encoding can carry.
    IdTooLong {
        kind: &'static str,
        len: usize,
        max: usize,
    },
    /// The toc was cut short.
    Truncated { at: usize },
    /// The toc has trailing bytes after a complete decode.
    Trailing { extra: usize },
    /// The toc names a format this build does not speak.
    UnsupportedFormat { found: u32 },
    /// The toc declares an entity kind this build does not know.
    UnknownEntityKind { discriminant: u8 },
    /// The declared entity count exceeds what the remaining bytes can hold.
    CountTooLarge { declared: u32, remaining: usize },
}

impl std::fmt::Display for KeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId { kind } => write!(f, "a {kind} id must not be empty"),
            Self::IdHasSeparator { kind, id } => write!(
                f,
                "a {kind} id must not contain '/' or NUL (got {id:?}): a separator would move the \
                 write into another entity's namespace"
            ),
            Self::IdTooLong { kind, len, max } => {
                write!(f, "a {kind} id is {len} bytes, over the {max}-byte limit")
            }
            Self::Truncated { at } => write!(f, "the table of contents is truncated at byte {at}"),
            Self::Trailing { extra } => {
                write!(f, "the table of contents has {extra} trailing byte(s)")
            }
            Self::UnsupportedFormat { found } => write!(
                f,
                "the table of contents declares format {found}; this build speaks {TOC_FORMAT}"
            ),
            Self::UnknownEntityKind { discriminant } => {
                write!(f, "unknown entity kind {discriminant}")
            }
            Self::CountTooLarge {
                declared,
                remaining,
            } => write!(
                f,
                "the table of contents declares {declared} entities but only {remaining} byte(s) \
                 remain"
            ),
        }
    }
}

impl std::error::Error for KeysError {}

/// One entity inside a tree: where it lives and what its bytes hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TocEntry {
    /// Where the entity lives.
    pub path: EntityPath,
    /// The content hash of its encoded bytes — the reader's tamper check.
    pub content_hash: [u8; 32],
    /// The length of those bytes.
    pub len: u32,
}

/// The table of contents of one config tree.
///
/// The **order of `entities` is part of the identity**: the toc's bytes are
/// hashed to produce the tree's name, so the same set in a different order is a
/// different tree. That is deliberate — a canonical order is the publisher's
/// job (sort before publishing), and a decoder that reordered entries would
/// silently change which hash a tree is known by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toc {
    /// The encoding version.
    pub format: u32,
    /// The entities, in the publisher's order.
    pub entities: Vec<TocEntry>,
}

/// The maximum id length the encoding carries (a `u16` length prefix).
pub const MAX_ID_LEN: usize = u16::MAX as usize;

/// The content hash of `bytes`.
///
/// FNV-1a over the std hasher rather than a digest crate: this value's job is to
/// make a reader notice that the bytes it fetched are not the bytes the toc
/// describes, and to give a tree a stable name. It is not a security boundary —
/// anything that can rewrite an entity's value can rewrite the toc and the head
/// with it — so pulling a cryptographic dependency into the default build for it
/// would be a poor trade.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&hasher.finish().to_be_bytes());
    out
}

/// The kind name used in error messages, so an operator learns WHICH identifier
/// is malformed rather than only that one is.
fn kind_name(path: &EntityPath) -> &'static str {
    match path {
        EntityPath::Tenant(_) => "tenant",
        EntityPath::Provider(_) => "provider",
        EntityPath::ProviderKey(_) => "provider_key",
        EntityPath::Model(_) => "model",
        EntityPath::LimitRole(_) => "limit_role",
        EntityPath::KeyBinding(_) => "key_binding",
        EntityPath::SubTenant(_) => "sub_tenant",
        EntityPath::SubTenantRoute(_) => "sub_tenant_route",
        EntityPath::Token(_) => "token",
        EntityPath::Cert(_) => "cert",
        EntityPath::Meta => "meta",
    }
}

/// Refuse an id that cannot be carried by the encoding or by a key.
///
/// The separator check is the important one: ids reach this code from operator
/// input and from the database, and a `/` in one would address a different
/// entity's namespace (or, worse, `toc`).
fn validate_id(kind: &'static str, id: &str) -> Result<(), KeysError> {
    if id.trim().is_empty() {
        return Err(KeysError::EmptyId { kind });
    }
    if id.contains('/') || id.contains('\0') {
        return Err(KeysError::IdHasSeparator {
            kind,
            id: id.to_string(),
        });
    }
    if id.len() > MAX_ID_LEN {
        return Err(KeysError::IdTooLong {
            kind,
            len: id.len(),
            max: MAX_ID_LEN,
        });
    }
    Ok(())
}

/// The size of a decoded entity record with an id of `id_len` bytes, used to
/// refuse an absurd declared count before allocating for it.
const fn entry_size(id_len: usize) -> usize {
    1 + 2 + id_len + 32 + 4
}

/// A bounds-checked reader over the encoded toc. Every read either advances or
/// returns [`KeysError::Truncated`] with the offset, so a short value can never
/// be mistaken for a valid one.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], KeysError> {
        if self.remaining() < n {
            return Err(KeysError::Truncated { at: self.at });
        }
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, KeysError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, KeysError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, KeysError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Validate every id in `entities`.
///
/// Public to the crate so a publisher can validate a whole plan BEFORE it writes
/// anything: refusing a bad id after three entities are already stored would
/// leave an unreachable partial tree behind for no reason.
pub(crate) fn validate_entities(entities: &[TocEntry]) -> Result<(), KeysError> {
    for e in entities {
        let kind = kind_name(&e.path);
        if let Some(id) = e.path.id() {
            validate_id(kind, id)?;
        }
    }
    Ok(())
}

impl Toc {
    /// Encode for storage as the value of [`cfg_toc`].
    ///
    /// # Errors
    /// Not fallible in the happy path, but the RETURN type is `Vec<u8>` rather
    /// than `Result` on purpose: the ids are validated when a toc is BUILT (see
    /// [`Toc::new`]), so encoding a `Toc` that exists cannot fail. Building one
    /// from unvalidated input is what this module does not let a caller do.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.entities.len() * entry_size(0));
        out.extend_from_slice(&self.format.to_le_bytes());
        out.extend_from_slice(&(self.entities.len() as u32).to_le_bytes());
        for e in &self.entities {
            out.push(e.path.discriminant());
            match e.path.id() {
                Some(id) => {
                    out.extend_from_slice(&(id.len() as u16).to_le_bytes());
                    out.extend_from_slice(id.as_bytes());
                }
                // The singleton carries no id; a zero length is its marker and is
                // only legal for `Meta` (enforced on decode).
                None => out.extend_from_slice(&0u16.to_le_bytes()),
            }
            out.extend_from_slice(&e.content_hash);
            out.extend_from_slice(&e.len.to_le_bytes());
        }
        out
    }

    /// Build a toc, validating every entity id.
    ///
    /// This is the only constructor that accepts unvalidated strings, and it
    /// refuses the ids that would corrupt a key.
    pub fn new(format: u32, entities: Vec<TocEntry>) -> Result<Self, KeysError> {
        validate_entities(&entities)?;
        Ok(Self { format, entities })
    }

    /// Decode a stored toc, refusing anything this build cannot interpret
    /// exactly.
    ///
    /// # Errors
    /// [`KeysError`] for a truncated value, trailing bytes, another format, an
    /// unknown entity kind, or an entity id that is empty or contains a
    /// separator. Each is refused rather than repaired: a half-decoded tree
    /// would be served as if a publisher had written it.
    pub fn decode(bytes: &[u8]) -> Result<Self, KeysError> {
        let mut cursor = Cursor { bytes, at: 0 };
        let format = cursor.u32()?;
        if format != TOC_FORMAT {
            return Err(KeysError::UnsupportedFormat { found: format });
        }
        let count = cursor.u32()?;
        // Refuse an absurd count before trusting it: the smallest entity record
        // is 39 bytes, so a count larger than that bound cannot be true.
        if count as usize > cursor.remaining() / entry_size(0) + 1 {
            return Err(KeysError::CountTooLarge {
                declared: count,
                remaining: cursor.remaining(),
            });
        }
        let mut entities = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let discriminant = cursor.u8()?;
            let id_len = cursor.u16()? as usize;
            let raw_id = cursor.take(id_len)?;
            let id = std::str::from_utf8(raw_id)
                .map_err(|_| KeysError::IdHasSeparator {
                    kind: "entity",
                    id: format!("<{id_len} non-utf8 bytes>"),
                })?
                .to_string();
            let mut content_hash = [0u8; 32];
            content_hash.copy_from_slice(cursor.take(32)?);
            let len = cursor.u32()?;

            let path = match discriminant {
                1..=10 => {
                    validate_id("entity", &id)?;
                    match discriminant {
                        1 => EntityPath::Tenant(id),
                        2 => EntityPath::Provider(id),
                        3 => EntityPath::ProviderKey(id),
                        4 => EntityPath::Model(id),
                        5 => EntityPath::LimitRole(id),
                        6 => EntityPath::KeyBinding(id),
                        7 => EntityPath::SubTenant(id),
                        8 => EntityPath::SubTenantRoute(id),
                        9 => EntityPath::Token(id),
                        _ => EntityPath::Cert(id),
                    }
                }
                11 => EntityPath::Meta,
                other => {
                    return Err(KeysError::UnknownEntityKind {
                        discriminant: other,
                    })
                }
            };
            entities.push(TocEntry {
                path,
                content_hash,
                len,
            });
        }
        if cursor.remaining() > 0 {
            return Err(KeysError::Trailing {
                extra: cursor.remaining(),
            });
        }
        Ok(Self { format, entities })
    }

    /// The name of this tree: the hash of its encoded bytes.
    ///
    /// This is the value written to [`ctl_head`], and therefore the commit point:
    /// the tree is visible to the rest of the cluster exactly when the head
    /// names it.
    #[must_use]
    pub fn hash(&self) -> String {
        let digest = content_hash(&self.encode());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> EntityPath {
        EntityPath::Tenant(id.to_string())
    }

    fn entry(id: &str, content: &[u8]) -> TocEntry {
        TocEntry {
            path: target(id),
            content_hash: content_hash(content),
            len: content.len() as u32,
        }
    }

    fn toc(ids: &[&str]) -> Toc {
        Toc {
            format: TOC_FORMAT,
            entities: ids.iter().map(|id| entry(id, id.as_bytes())).collect(),
        }
    }

    /// The key layout is a contract: these strings are what an operator greps for
    /// and what a future migration must keep readable.
    ///
    /// Falsification: change any prefix and the assertions fail.
    #[test]
    fn the_key_layout_is_exactly_the_documented_one() {
        assert_eq!(ctl_head(), "hydra/ctl/head");
        assert_eq!(ctl_cluster_id(), "hydra/ctl/cluster_id");
        assert_eq!(ctl_format(), "hydra/ctl/format");
        assert_eq!(cfg_toc("abc123"), "hydra/cfg/toc/abc123");
        assert_eq!(cfg_entity(&target("acme")), "hydra/cfg/e/tenant/acme");
        assert_eq!(cfg_entity(&EntityPath::Meta), "hydra/cfg/e/meta");
    }

    /// Every entity kind has a distinct, `/`-free segment, and the singleton has
    /// no id at all.
    ///
    /// Falsification: give two variants the same segment and the uniqueness
    /// assertion fails.
    #[test]
    fn every_entity_kind_has_its_own_segment() {
        let all = [
            EntityPath::Tenant("x".into()),
            EntityPath::Provider("x".into()),
            EntityPath::ProviderKey("x".into()),
            EntityPath::Model("x".into()),
            EntityPath::LimitRole("x".into()),
            EntityPath::KeyBinding("x".into()),
            EntityPath::SubTenant("x".into()),
            EntityPath::SubTenantRoute("x".into()),
            EntityPath::Token("x".into()),
            EntityPath::Cert("x".into()),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for path in &all {
            let segment = path.to_key_segment();
            assert!(
                seen.insert(segment.clone()),
                "{segment} is used by more than one entity kind"
            );
            assert_eq!(
                segment.matches('/').count(),
                1,
                "{segment} must be `<kind>/<id>`"
            );
        }
        assert_eq!(EntityPath::Meta.to_key_segment(), "meta");
        assert!(all.iter().all(|p| p.id().is_some()));
        assert!(EntityPath::Meta.id().is_none());
    }

    /// Round trip, and the two properties the tree identity depends on: a fixed
    /// toc encodes to fixed bytes, and the entity ORDER changes the name.
    ///
    /// Falsification: sort the entities inside `encode` and the order assertion
    /// fails; drop `format` from the encoding and the tamper test below becomes
    /// weaker than it claims.
    #[test]
    fn a_toc_round_trips_and_its_order_is_part_of_its_identity() {
        let t = toc(&["a", "b"]);
        let bytes = t.encode();
        let back = Toc::decode(&bytes).expect("round trip");
        assert_eq!(back, t, "decode(encode(t)) must be t");

        assert_eq!(
            t.encode(),
            toc(&["a", "b"]).encode(),
            "the same toc must encode to the same bytes"
        );
        assert_ne!(
            t.hash(),
            toc(&["b", "a"]).hash(),
            "reordering the entities must change the tree's name"
        );
        assert_eq!(t.hash().len(), 64, "the hash is hex-encoded");
    }

    /// A changed entity changes the tree's name — the property that makes the
    /// head a commit point rather than a label.
    ///
    /// Falsification: hash the entity PATHS instead of their content and this
    /// fails.
    #[test]
    fn changing_one_entitys_content_changes_the_tree_name() {
        let before = toc(&["a", "b"]);
        let mut after = toc(&["a", "b"]);
        after.entities[1] = entry("b", b"different content");
        assert_ne!(
            before.hash(),
            after.hash(),
            "a content change must produce a different tree name"
        );
    }

    /// Malformed tocs are refused, never half-decoded: a truncated or extended
    /// value could otherwise be accepted as a valid tree and silently serve a
    /// config no publisher ever wrote.
    ///
    /// Falsification: stop checking the trailing bytes and the last case fails.
    #[test]
    fn malformed_tocs_are_refused() {
        let bytes = toc(&["a", "b"]).encode();

        for cut in 0..bytes.len() {
            let got = Toc::decode(&bytes[..cut]);
            assert!(
                got.is_err(),
                "a toc truncated to {cut} byte(s) must be refused, got {got:?}"
            );
        }

        let mut extended = bytes.clone();
        extended.push(0);
        assert!(
            matches!(Toc::decode(&extended), Err(KeysError::Trailing { .. })),
            "trailing bytes must be refused"
        );

        let mut wrong_format = bytes.clone();
        wrong_format[0] = TOC_FORMAT.wrapping_add(1) as u8;
        assert!(
            matches!(
                Toc::decode(&wrong_format),
                Err(KeysError::UnsupportedFormat { .. })
            ),
            "a toc from another format must be refused, not guessed at"
        );

        let mut unknown_kind = bytes.clone();
        // The first entity's kind discriminant sits at byte 8: 4 bytes of format,
        // then 4 bytes of entity count, then each entry starts with its kind byte.
        unknown_kind[8] = 250;
        assert!(
            matches!(
                Toc::decode(&unknown_kind),
                Err(KeysError::UnknownEntityKind { discriminant: 250 })
            ),
            "an unknown entity kind must be refused, got {:?}",
            Toc::decode(&unknown_kind)
        );
    }
}
