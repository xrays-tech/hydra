# 计划：入口 api-key 多形式解析（Inbound Credential Transports）

> **状态**：待执行（设计与契约已于 2026-09-10 与用户逐条确认）
> **触发**：编程 Agent / SDK 用不同 HTTP 形式携带 api-key（Gemini CLI 的 `x-goog-api-key`、Azure 风格的 `api-key`、浏览器 WebSocket 的 `?key=`）。当前网关只认 `Authorization: Bearer` 与 `x-api-key`，这类客户端一律 401 `missing_api_key`。
> **范围**：`hydra-core` 新增纯解析模块 + `hydra-server` 薄适配 + 测试 + 权威文档同步。**零 schema / 零 admin API / 零 UI 改动。**
> **不涉及**：上游注入形式（Hydra → provider 仍为 `Authorization: Bearer`，见 §9）。

---

## 1. 问题与现状基线

| # | 事实 | 证据 |
|---|---|---|
| B1 | 入口只解析两种形式：`Authorization: Bearer …` 与 `x-api-key`；其余一律 401 | `crates/hydra-server/src/proxy.rs:158-181`（`extract_api_key`）、`proxy.rs:377-380`（`missing_api_key`） |
| B2 | 只有一个提取点，所有请求（含 `GET /v1/models` 的目录收窄）共用 | `proxy.rs:327`（唯一调用点，全仓 grep 确认） |
| B3 | 解析结果的下游用途与来源形式无关：外部 `auth_url` 鉴权、key-prefix 绑定（raw prefix）、usage 脱敏记录 | `proxy.rs:384`、`router::match_key_binding`（`proxy.rs:1118`）、`mask_key`（`proxy.rs:446`） |
| B4 | proxy 路径**不打印 URI**，query 形式的 key 不会进 Hydra 日志 | proxy 内无任何带 uri/url 的 `warn!/info!/debug!`（唯一命中在 `cluster/control_client.rs` 与 `main.rs`，非数据面） |
| B5 | 上游 URL 只由 `uri.path()` 构造，**query 不会转发到上游** | `proxy.rs:393`（`req_header.uri.path()`）+ `hydra-core/src/rewrite.rs:38`（`rewrite_path` 只吃 path） |
| B6 | 集群转发只用于 **admin** 变更请求，数据面不转发 | `cluster/forward.rs:93-116`（`forward_mutation`，仅转发 authorization/content-type）；`cluster/mod.rs:1-16`（edge 为无状态数据面，自持快照） |
| B7 | core 依赖白名单：`serde/serde_json/memchr/bytes/sha2`，禁 tokio/pingora/sqlx/reqwest/hyper | `crates/hydra-core/src/lib.rs:11-17` + `tests/compile_gate.rs` |
| B8 | 401 响应体形状固定，可作断言 | `proxy.rs:1092-1099` |
| B9 | 集成测试骨架齐备（wiremock 双服务器 + 真 Pingora 线程） | `crates/hydra-server/tests/terminate_mode.rs:64-239`、`allowing_auth_server`（`:1512`） |

**核心缺口**：`x-goog-api-key`、`api-key`、裸 `Authorization`、query 四种形式现在全部 401，Agent 接入即失败。

---

## 2. 决策记录（用户确认）

| # | 决策 | 取舍 |
|---|---|---|
| D1 | **只做入口多形式解析**，不动上游注入 | 上游形式（Anthropic `x-api-key`、Gemini `x-goog-api-key`）是另一个独立缺口，见 §9 |
| D2 | 新增形式：`x-goog-api-key`、`api-key`、裸 `Authorization`、query `?key=/?api_key=`；**不含 Cookie** | Cookie 有 CSRF 面，网关侧不引入 |
| D3 | **全部无条件接受，不加开关**（无租户级白名单） | 零 schema 变更；query 的安全边界由 C6 的就地约束兜住 |
| D4 | 多来源冲突：**固定优先级取第一个非空**，值不一致时只记一条 warn | 不误伤同时发两个头的正常客户端，异常可观测 |

---

## 3. 契约（实现必须满足的外部可观察行为）

### C1 来源与固定优先级（取第一个"有效"值）

| 序 | 来源 | 取值规则 |
|---|---|---|
| 1 | `Authorization: Bearer <k>` | scheme 大小写不敏感（RFC 9110）；值 trim 后非空才算有效 |
| 2 | `Authorization: <k>`（裸值） | 头值 trim 后**不含任何空白** → 整体为 key |
| 3 | `x-api-key` | trim 后非空 |
| 4 | `api-key` | trim 后非空（Azure 风格） |
| 5 | `x-goog-api-key` | trim 后非空（Gemini 风格） |
| 6 | query 参数 | 名称依次 `key` → `api_key` → `apikey` → `access_token`，取第一个"存在且非空"；值做 `%XX` 百分号解码 |

### C2 冲突
所有有效来源都参与判定；返回首选值 + "存在但值不同"的来源列表（声明序靠后的来源）。shell 对这些**不同的**来源打一条 `warn!`（含 `trace_id` + 来源标签，**绝不含 key 值**），请求照常放行。

### C3 缺失
所有来源无效 → 行为不变：401 `missing_api_key`（B8 响应体形状不变）。

### C4 规范化
头值 trim 首尾空白；空串按"未提供"处理（保住现有"`Bearer ` 空值继续下探"语义）；key 值大小写敏感，不折叠；query 值 `%XX` 解码，**`+` 不转空格**（api-key 可能含 `+`），非法转义按字面保留。

### C5 下游语义不变
解析结果照样用于 (a) 外部 `auth_url` 鉴权、(b) key-prefix 绑定、(c) usage 的 `client_api_key_masked`；与来源形式无关。

### C6 安全边界（query 形式的必要配套，用测试锁死）
- query 里的 key **不进 Hydra 日志**（B4 现状，不得回退）；
- query **不转发上游**（B5 现状：上游 URL 只有 path）；
- 错误响应体不回显 key（B8 现状）。

### C7 明确不在范围
上游注入形式；query 中**其它**参数向上游透传（现状丢弃，独立问题）；Cookie；SigV4 / OAuth 动态取 token / mTLS；租户级开关。

### C8 已知歧义（有意接受）
同时存在 `Authorization`（可能是 Vertex OAuth token）与 `x-goog-api-key`（AI Studio key）时按 C1 取前者。这是"第一个非空胜出"的直接后果，冲突 warn 会留痕。

---

## 4. TDD Route

```text
TDD Route:
- Mode: off
- Decision: skipped
- Strict authority: not applicable（用户/项目未要求 strict；Aegis 未激活）
- Test posture: post-change regression（core 纯函数矩阵单测与实现同任务编写；shell 侧走集成回归）
- Reason: 纯函数 + 有限优先级矩阵；项目 CI 硬门是 clippy -D warnings + 全量测试，而非 RED-first 流程
- Verification: cargo test -p hydra-core；cargo test -p hydra-server --features server；clippy --all-targets -D warnings
```
---

## 5. 文件地图

| 文件 | 动作 | 边界 |
|---|---|---|
| `crates/hydra-core/src/apikey.rs` | **新建** | 纯解析器 + 单测矩阵；无新依赖（只吃 `&str`） |
| `crates/hydra-core/src/lib.rs` | 改 1 行 | `pub mod apikey;`（插在 `pub mod auth;` 前，保持字母序） |
| `crates/hydra-server/src/proxy.rs` | 改 3 处 | import 块（:60 同组）；`extract_api_key`（158-181）；调用点（324-327）。其余逻辑零改动 |
| `crates/hydra-server/src/proxy/ctx.rs` | 改注释 1 处 | `client_api_key` 字段文档 |
| `crates/hydra-server/tests/terminate_mode.rs` | 追加 | 2 个 helper + 6 个集成用例 |
| `dev-docs/design.md` | 改 3 处 | §6.3 第 3 步（611 行）、§7.1b（742 行）、§6.5（655 行） |
| `dev-docs/ops.md` | 改 1 处 | 排障条目（495 行） |
| `dev-docs/aegis/INDEX.md` | 追加 1 行 | 工作区索引 |

---

## 6. 任务拆解

### T1 — core 解析模块（新建 `crates/hydra-core/src/apikey.rs`）

```rust
//! Client credential extraction across HTTP transport forms (pure).
//!
//! Hydra accepts a client api-key from any of the transports real coding
//! agents and SDKs use
//! (dev-docs/aegis/plans/2026-09-10-inbound-credential-forms.md):
//!
//! | # | Transport | Who uses it |
//! |---|-----------|-------------|
//! | 1 | `Authorization: Bearer <k>` | OpenAI SDK, Anthropic SDK (OAuth) |
//! | 2 | `Authorization: <k>` (bare, no scheme) | self-rolled clients |
//! | 3 | `x-api-key` | Anthropic SDK |
//! | 4 | `api-key` | Azure OpenAI |
//! | 5 | `x-goog-api-key` | Gemini CLI / google-genai |
//! | 6 | query `?key=` / `?api_key=` / `?apikey=` / `?access_token=` | browser WebSocket |
//!
//! Precedence is the table order: the first transport that yields a non-empty
//! value wins. Any *later* transport carrying a **different** value is reported
//! in [`KeyExtraction::conflicts`] so the I/O shell can log one warning — the
//! values themselves are never logged.
//!
//! Pure: no I/O, no time, no global state, no allocation beyond the returned
//! `String`.

/// Where a client api-key was found. Variant order == precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// `Authorization: Bearer <k>` (scheme case-insensitive).
    AuthorizationBearer,
    /// `Authorization: <k>` with no whitespace anywhere (no scheme at all).
    AuthorizationBare,
    /// `x-api-key`.
    XApiKey,
    /// `api-key` (Azure OpenAI style).
    ApiKey,
    /// `x-goog-api-key` (Gemini style).
    XGoogApiKey,
    /// Query-string parameter.
    Query,
}

impl KeySource {
    /// Stable label for logs/metrics — never contains the key value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::AuthorizationBearer => "authorization_bearer",
            Self::AuthorizationBare => "authorization_bare",
            Self::XApiKey => "x_api_key",
            Self::ApiKey => "api_key",
            Self::XGoogApiKey => "x_goog_api_key",
            Self::Query => "query",
        }
    }
}

/// Query parameter names searched, in order. First present non-empty wins.
const QUERY_KEY_NAMES: [&str; 4] = ["key", "api_key", "apikey", "access_token"];

/// Raw request-derived inputs.
///
/// Header lookup is the I/O shell's job (core carries no HTTP types); values
/// are passed **verbatim**, un-trimmed, and normalised inside the parser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientKeyInput<'a> {
    pub authorization: Option<&'a str>,
    pub x_api_key: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub x_goog_api_key: Option<&'a str>,
    /// Raw query string without the leading `?` (i.e. `Uri::query()`).
    pub query: Option<&'a str>,
}

/// Extraction result (design §6.3 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyExtraction {
    /// Highest-precedence key, if any transport carried one.
    pub key: Option<String>,
    /// Which transport produced [`Self::key`].
    pub source: Option<KeySource>,
    /// Other transports that carried a **different** non-empty value, in
    /// ascending precedence order. Never contains key values.
    pub conflicts: Vec<KeySource>,
}

/// Extract the client api-key from `input`, honouring the precedence table.
#[must_use]
pub fn extract_client_key(input: &ClientKeyInput<'_>) -> KeyExtraction {
    let mut found: Vec<(KeySource, String)> = Vec::new();

    if let Some((source, value)) = parse_authorization(input.authorization) {
        found.push((source, value.to_string()));
    }
    for (source, raw) in [
        (KeySource::XApiKey, input.x_api_key),
        (KeySource::ApiKey, input.api_key),
        (KeySource::XGoogApiKey, input.x_goog_api_key),
    ] {
        if let Some(value) = raw.and_then(trimmed_non_empty) {
            found.push((source, value.to_string()));
        }
    }
    if let Some(value) = input.query.and_then(query_key) {
        found.push((KeySource::Query, value));
    }

    let Some((source, key)) = found.first().map(|(s, v)| (*s, v.clone())) else {
        return KeyExtraction::default();
    };
    let conflicts = found
        .iter()
        .skip(1)
        .filter(|(_, value)| *value != key)
        .map(|(source, _)| *source)
        .collect();
    KeyExtraction {
        key: Some(key),
        source: Some(source),
        conflicts,
    }
}

/// Parse an `Authorization` header value.
///
/// - `Bearer <k>` (scheme case-insensitive per RFC 9110) → bearer source;
/// - a value containing **no** whitespace (and not a lone scheme name) →
///   bare-key source;
/// - anything else (`Basic …`, `Api-Key …`, a scheme name with no value, an
///   empty value) → `None`, so the caller falls through to the next
///   transport (C4).
fn parse_authorization(raw: Option<&str>) -> Option<(KeySource, &str)> {
    let value = raw?.trim();
    if value.is_empty() {
        return None;
    }
    match value.find(char::is_whitespace) {
        Some(idx) => {
            let (scheme, rest) = value.split_at(idx);
            if !scheme.eq_ignore_ascii_case("bearer") {
                return None;
            }
            let key = rest.trim();
            (!key.is_empty()).then_some((KeySource::AuthorizationBearer, key))
        }
        // No whitespace: a bare key — unless the value IS the scheme name
        // (`Authorization: Bearer` with the value trimmed away), which is not
        // a credential at all.
        None if value.eq_ignore_ascii_case("bearer") => None,
        None => Some((KeySource::AuthorizationBare, value)),
    }
}

/// Trim, mapping the empty string to "not provided".
fn trimmed_non_empty(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (!value.is_empty()).then_some(value)
}

/// First non-empty value among [`QUERY_KEY_NAMES`], percent-decoded.
fn query_key(query: &str) -> Option<String> {
    for name in QUERY_KEY_NAMES {
        for pair in query.split('&') {
            let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            if raw_name != name {
                continue;
            }
            let decoded = percent_decode(raw_value);
            let trimmed = decoded.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Percent-decode `%XX` escapes.
///
/// `+` is deliberately **left as-is** (never turned into a space): api-keys may
/// legitimately contain `+`, and every transport we accept uses `%XX`. Invalid
/// or truncated escapes are kept literally. Non-UTF-8 results degrade lossily
/// (no panic path).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if !bytes.contains(&b'%') {
        return raw.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hex digit → value, or `None` for a non-hex byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
```
### T2 — core 单测矩阵（同文件 `#[cfg(test)]` 模块）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        authorization: Option<&'a str>,
        x_api_key: Option<&'a str>,
        api_key: Option<&'a str>,
        x_goog_api_key: Option<&'a str>,
        query: Option<&'a str>,
    ) -> ClientKeyInput<'a> {
        ClientKeyInput {
            authorization,
            x_api_key,
            api_key,
            x_goog_api_key,
            query,
        }
    }

    #[test]
    fn no_credentials_anywhere_is_empty() {
        let out = extract_client_key(&input(None, None, None, None, None));
        assert_eq!(out, KeyExtraction::default());
    }

    #[test]
    fn bearer_wins_over_every_later_transport() {
        let out = extract_client_key(&input(
            Some("Bearer A"),
            Some("B"),
            Some("C"),
            Some("D"),
            Some("key=E"),
        ));
        assert_eq!(out.key.as_deref(), Some("A"));
        assert_eq!(out.source, Some(KeySource::AuthorizationBearer));
        assert_eq!(
            out.conflicts,
            vec![
                KeySource::XApiKey,
                KeySource::ApiKey,
                KeySource::XGoogApiKey,
                KeySource::Query
            ]
        );
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for header in ["Bearer k", "bearer k", "BEARER k", "BeArEr k"] {
            let out = extract_client_key(&input(Some(header), None, None, None, None));
            assert_eq!(out.key.as_deref(), Some("k"), "header: {header}");
            assert_eq!(out.source, Some(KeySource::AuthorizationBearer));
        }
    }

    #[test]
    fn bare_authorization_value_is_a_key() {
        let out = extract_client_key(&input(Some("sk-bare"), None, None, None, None));
        assert_eq!(out.key.as_deref(), Some("sk-bare"));
        assert_eq!(out.source, Some(KeySource::AuthorizationBare));
    }

    #[test]
    fn non_bearer_scheme_is_not_a_key_and_falls_through() {
        let out = extract_client_key(&input(
            Some("Basic dXNlcjpwYXNz"),
            Some("B"),
            None,
            None,
            None,
        ));
        assert_eq!(out.key.as_deref(), Some("B"));
        assert_eq!(out.source, Some(KeySource::XApiKey));
        assert!(
            out.conflicts.is_empty(),
            "a non-Bearer scheme is not a credential transport here"
        );
    }

    #[test]
    fn valueless_bearer_falls_through() {
        for header in ["Bearer", "Bearer   ", "bearer\t"] {
            let out = extract_client_key(&input(Some(header), Some("B"), None, None, None));
            assert_eq!(out.key.as_deref(), Some("B"), "header: {header:?}");
        }
    }

    #[test]
    fn header_values_are_trimmed_and_empties_skipped() {
        let out = extract_client_key(&input(None, Some("   "), Some("  sk-api  "), None, None));
        assert_eq!(out.key.as_deref(), Some("sk-api"));
        assert_eq!(out.source, Some(KeySource::ApiKey));
    }

    #[test]
    fn precedence_x_api_key_over_api_key_over_google() {
        let out = extract_client_key(&input(None, Some("A"), Some("B"), Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("A"));
        assert_eq!(out.conflicts, vec![KeySource::ApiKey, KeySource::XGoogApiKey]);

        let out = extract_client_key(&input(None, None, Some("B"), Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("B"));
        assert_eq!(out.conflicts, vec![KeySource::XGoogApiKey]);

        let out = extract_client_key(&input(None, None, None, Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("C"));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn identical_values_are_not_conflicts() {
        let out = extract_client_key(&input(
            Some("Bearer same"),
            Some("same"),
            None,
            None,
            Some("key=same"),
        ));
        assert_eq!(out.key.as_deref(), Some("same"));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn query_names_are_searched_in_order() {
        let out = extract_client_key(&input(
            None,
            None,
            None,
            None,
            Some("access_token=A&apikey=B&key=C"),
        ));
        assert_eq!(out.key.as_deref(), Some("C"));
        assert_eq!(out.source, Some(KeySource::Query));

        let out = extract_client_key(&input(
            None,
            None,
            None,
            None,
            Some("access_token=A&apikey=B"),
        ));
        assert_eq!(out.key.as_deref(), Some("B"));

        let out = extract_client_key(&input(None, None, None, None, Some("access_token=A")));
        assert_eq!(out.key.as_deref(), Some("A"));
    }

    #[test]
    fn query_skips_empty_values_and_keeps_searching() {
        let out = extract_client_key(&input(None, None, None, None, Some("key=&api_key=real")));
        assert_eq!(out.key.as_deref(), Some("real"));

        let out = extract_client_key(&input(None, None, None, None, Some("key&api_key=real")));
        assert_eq!(out.key.as_deref(), Some("real"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=")));
        assert_eq!(out.key, None);
    }

    #[test]
    fn query_values_are_percent_decoded_but_plus_is_literal() {
        let out = extract_client_key(&input(None, None, None, None, Some("key=sk%2Da%20b")));
        assert_eq!(out.key.as_deref(), Some("sk-a b"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=a+b")));
        assert_eq!(out.key.as_deref(), Some("a+b"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=%zz%")));
        assert_eq!(
            out.key.as_deref(),
            Some("%zz%"),
            "invalid escapes stay literal"
        );
    }

    #[test]
    fn falls_back_to_query_only_when_no_header_carried_a_key() {
        let out = extract_client_key(&input(Some("Basic x"), Some(""), None, None, Some("key=Q")));
        assert_eq!(out.key.as_deref(), Some("Q"));
        assert_eq!(out.source, Some(KeySource::Query));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn source_labels_are_stable_and_value_free() {
        assert_eq!(KeySource::AuthorizationBearer.label(), "authorization_bearer");
        assert_eq!(KeySource::AuthorizationBare.label(), "authorization_bare");
        assert_eq!(KeySource::XApiKey.label(), "x_api_key");
        assert_eq!(KeySource::ApiKey.label(), "api_key");
        assert_eq!(KeySource::XGoogApiKey.label(), "x_goog_api_key");
        assert_eq!(KeySource::Query.label(), "query");
    }
}
```

### T3 — core 导出

`crates/hydra-core/src/lib.rs`：在 `pub mod auth;` 之前插入

```rust
pub mod apikey;
```

依赖闸门未触碰（未新增任何依赖）：`cargo test -p hydra-core --test compile_gate`。

### T4 — shell 薄适配（`crates/hydra-server/src/proxy.rs`）

**T4.1** import：在 `use hydra_core::swrr;`（`proxy.rs:60`）同组追加

```rust
use hydra_core::apikey::{extract_client_key, ClientKeyInput, KeyExtraction};
```

**T4.2** 整体替换 `extract_api_key`（`proxy.rs:158-181`）：

```rust
    /// Parse the client api-key from **every** supported transport, in the
    /// precedence order documented by `hydra_core::apikey` (design §6.3 §3).
    ///
    /// The shell owns header lookup (core carries no HTTP types). Values are
    /// handed over verbatim; normalisation and conflict detection live in the
    /// pure parser. The returned [`KeyExtraction`] carries the conflict list so
    /// the caller can log one warning — key values are never logged.
    fn extract_api_key(session: &Session) -> KeyExtraction {
        let header = session.req_header();
        let headers = &header.headers;
        let lookup = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        extract_client_key(&ClientKeyInput {
            authorization: lookup("authorization"),
            x_api_key: lookup("x-api-key"),
            api_key: lookup("api-key"),
            x_goog_api_key: lookup("x-goog-api-key"),
            query: header.uri.query(),
        })
    }
```

**T4.3** 调用点（`proxy.rs:324-327`）替换为：

```rust
        // (3) Client api-key parse (§6.3 §3). Mandatory for every non-catalog
        //     request; OPTIONAL for `GET /v1/models` — there a presented key
        //     is never validated, it only narrows the catalog (2.5 below).
        //     Every supported transport is accepted; when several transports
        //     disagree the highest-precedence one wins and we log ONE warning
        //     (labels only, never values) —
        //     dev-docs/aegis/plans/2026-09-10-inbound-credential-forms.md.
        let extraction = Self::extract_api_key(session);
        if !extraction.conflicts.is_empty() {
            warn!(
                trace_id = %ctx.trace_id,
                source = extraction.source.map_or("none", |s| s.label()),
                conflicting = ?extraction
                    .conflicts
                    .iter()
                    .map(|s| s.label())
                    .collect::<Vec<_>>(),
                "client presented differing api-key values in several transports; \
                 using the highest-precedence one"
            );
        }
        let api_key_opt = extraction.key;
```

其余行（`is_catalog_get`、`match api_key_opt`、`ctx.client_api_key`）**不动**。
### T5 — 集成测试（`crates/hydra-server/tests/terminate_mode.rs` 追加）

**T5.1** 两个 helper（放在 `send_until_ready`（`:251`）之后）：

```rust
/// POST the standard chat body carrying an explicit set of credential
/// transports, retrying until the proxy is ready (Pingora binds async).
async fn send_with_headers(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> reqwest::Response {
    let mut last_err = None;
    for _ in 0..60 {
        let mut req = client.post(url).header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        match req.body(body.to_string()).send().await {
            Ok(r) => return r,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    panic!(
        "proxy never became ready: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    );
}

/// The bodies the tenant auth service actually received, concatenated. Proves
/// the transport (not merely the header name) was understood — the auth
/// service is a real wiremock server (dev-plan §1 铁律 2).
async fn auth_seen_bodies(auth_server: &MockServer) -> String {
    auth_server
        .received_requests()
        .await
        .expect("auth recording on")
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}
```

**T5.2** 每个用例都走 **fresh graph**（新 pool + 新 state + 新 proxy + 新 mock），否则 auth 缓存（TTL 300s）会把第 2 个形式短路成缓存命中，断言失去意义。

```rust
/// Every supported **header** transport authenticates and the raw key reaches
/// the tenant auth service (fresh graph per case: the auth cache must not be
/// able to mask a parse failure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_header_credential_transport_authenticates() {
    let body = r#"{"model":"gpt-4","messages":[]}"#;
    let cases: [(&str, &str, &str); 5] = [
        ("authorization bearer", "authorization", "Bearer test-client-key"),
        ("authorization bare", "authorization", "test-client-key"),
        ("x-api-key", "x-api-key", "test-client-key"),
        ("api-key", "api-key", "test-client-key"),
        ("x-goog-api-key", "x-goog-api-key", "test-client-key"),
    ];

    for (label, name, value) in cases {
        let auth_server = allowing_auth_server().await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-upstream-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(&upstream)
            .await;

        let pool = common::setup_pool().await;
        seed_one(&pool, &format!("{}/auth", auth_server.uri()), &upstream.uri()).await;
        let root = start_proxy(build_state(&pool).await);
        let client = test_client();

        let resp = send_with_headers(
            &client,
            &format!("{root}/v1/chat/completions"),
            &[(name, value)],
            body,
        )
        .await;
        assert_eq!(resp.status(), 200, "transport {label} must authenticate");

        let seen = auth_seen_bodies(&auth_server).await;
        assert!(
            seen.contains("test-client-key"),
            "transport {label}: the tenant auth service must receive the raw key, saw: {seen}"
        );

        let received = upstream
            .received_requests()
            .await
            .expect("upstream recording on");
        assert_eq!(received.len(), 1, "transport {label}: exactly one upstream call");
    }
}

/// C6: the query transport authenticates, and the credential query string is
/// NEVER forwarded upstream (the upstream URL is built from `uri.path()`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_credential_transport_authenticates_and_is_not_forwarded() {
    let auth_server = allowing_auth_server().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-upstream-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(&pool, &format!("{}/auth", auth_server.uri()), &upstream.uri()).await;
    let root = start_proxy(build_state(&pool).await);
    let client = test_client();

    let resp = send_with_headers(
        &client,
        &format!("{root}/v1/chat/completions?key=test-client-key"),
        &[],
        r#"{"model":"gpt-4","messages":[]}"#,
    )
    .await;
    assert_eq!(resp.status(), 200, "the query transport must authenticate");

    let seen = auth_seen_bodies(&auth_server).await;
    assert!(
        seen.contains("test-client-key"),
        "the tenant auth service must receive the key: {seen}"
    );

    let received = upstream
        .received_requests()
        .await
        .expect("upstream recording on");
    assert_eq!(received.len(), 1);
    assert!(
        received[0].url.query().is_none(),
        "the credential query string must never reach the provider: {}",
        received[0].url
    );
}

/// C2/D4: differing transports still route, using the highest-precedence value —
/// and the shadowing value never leaves the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn differing_transports_use_the_highest_precedence_value() {
    let auth_server = allowing_auth_server().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(&pool, &format!("{}/auth", auth_server.uri()), &upstream.uri()).await;
    let root = start_proxy(build_state(&pool).await);
    let client = test_client();

    let resp = send_with_headers(
        &client,
        &format!("{root}/v1/chat/completions"),
        &[
            ("authorization", "Bearer test-client-key"),
            ("x-api-key", "sk-shadow"),
        ],
        r#"{"model":"gpt-4","messages":[]}"#,
    )
    .await;
    assert_eq!(resp.status(), 200, "conflicting transports must still route");

    let seen = auth_seen_bodies(&auth_server).await;
    assert!(
        seen.contains("test-client-key"),
        "the highest-precedence value must be the one authenticated: {seen}"
    );
    assert!(
        !seen.contains("sk-shadow"),
        "the shadowed value must never reach the auth service: {seen}"
    );
}

/// Negative pin (C3 + B8): no credential in any transport ⇒ 401
/// `missing_api_key`, zero upstream, zero auth call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_credential_in_any_transport_is_401_missing_api_key() {
    let auth_server = allowing_auth_server().await;
    let upstream = MockServer::start().await;

    let pool = common::setup_pool().await;
    seed_one(&pool, &format!("{}/auth", auth_server.uri()), &upstream.uri()).await;
    let root = start_proxy(build_state(&pool).await);
    let client = test_client();

    let resp = send_with_headers(
        &client,
        &format!("{root}/v1/chat/completions"),
        &[],
        r#"{"model":"gpt-4","messages":[]}"#,
    )
    .await;
    assert_eq!(resp.status(), 401, "no credential ⇒ 401, unchanged");
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("\"message\":\"missing_api_key\""),
        "the 401 body shape must stay stable: {body}"
    );

    let received = upstream
        .received_requests()
        .await
        .expect("upstream recording on");
    assert!(received.is_empty(), "an unauthenticated call must not reach a provider");
    let auth_calls = auth_server
        .received_requests()
        .await
        .expect("auth recording on");
    assert!(auth_calls.is_empty(), "no key ⇒ no auth round-trip");
}

/// Negative pin (C1/C4): a non-Bearer scheme is not a credential transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_bearer_authorization_scheme_is_rejected() {
    let auth_server = allowing_auth_server().await;
    let upstream = MockServer::start().await;

    let pool = common::setup_pool().await;
    seed_one(&pool, &format!("{}/auth", auth_server.uri()), &upstream.uri()).await;
    let root = start_proxy(build_state(&pool).await);
    let client = test_client();

    let resp = send_with_headers(
        &client,
        &format!("{root}/v1/chat/completions"),
        &[("authorization", "Basic dXNlcjpwYXNz")],
        r#"{"model":"gpt-4","messages":[]}"#,
    )
    .await;
    assert_eq!(resp.status(), 401, "Basic is not an api-key transport");

    let received = upstream
        .received_requests()
        .await
        .expect("upstream recording on");
    assert!(received.is_empty());
}

/// C5 regression pin: the `GET /v1/models` directory is still narrowed by
/// key-prefix binding through ANY transport — here the query form. Same seed as
/// `catalog_get_v1_models_key_prefix_binding_restricts_providers`, but the key
/// travels in the query string (the catalog read itself stays anonymous-free:
/// external auth never runs for `GET /v1/models`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_narrowing_still_works_through_the_query_transport() {
    let auth_server = allowing_auth_server().await;
    let upstream_a = MockServer::start().await;
    let upstream_b = MockServer::start().await;

    let pool = common::setup_pool().await;
    seed_provider(&pool, "pA", "provA", "ProviderA", &upstream_a.uri()).await;
    seed_provider(&pool, "pB", "provB", "ProviderB", &upstream_b.uri()).await;
    for (mid, pid, key) in [
        ("m_a_alpha", "pA", "alpha"),
        ("m_a_shared", "pA", "shared"),
        ("m_b_beta", "pB", "beta"),
        ("m_b_shared", "pB", "shared"),
    ] {
        repo::insert_provider_model(
            &pool,
            &ProviderModel {
                id: mid.into(),
                key: key.into(),
                name: key.into(),
                provider_id: pid.into(),
                status: 1,
            },
        )
        .await
        .expect("insert provider_model");
    }
    seed_tenant(&pool, "t1", "localhost", &format!("{}/auth", auth_server.uri())).await;
    for (tpid, pid) in [("tp_a", "pA"), ("tp_b", "pB")] {
        repo::insert_tenant_provider(
            &pool,
            &TenantProvider {
                id: tpid.into(),
                tenant_id: "t1".into(),
                provider_id: pid.into(),
            },
        )
        .await
        .expect("insert tenant_provider");
    }
    seed_key(&pool, &StaticKeyProvider::new([1u8; 32], 1), "pk_a", "pA", "sk-a").await;
    seed_key(&pool, &StaticKeyProvider::new([1u8; 32], 1), "pk_b", "pB", "sk-b").await;
    repo::insert_provider_key_binding(
        &pool,
        &ProviderKeyBinding {
            id: "bind_test_client".into(),
            key_prefix: "test-client".into(),
            provider_id: "pA".into(),
            enabled: true,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider_key_binding");
    seed_default_role(&pool, "t1").await;

    let root = start_proxy(build_state(&pool).await);
    let client = test_client();

    // Anonymous helper: the credential travels in the query string only.
    let resp =
        get_until_ready_anonymous(&client, &format!("{root}/v1/models?key=test-client-key")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(
        body,
        r#"{"object":"list","data":[{"id":"alpha","object":"model"},{"id":"shared","object":"model"}]}"#,
        "the query transport must feed key-prefix binding exactly like a header: {body}"
    );
    let auth_calls = auth_server
        .received_requests()
        .await
        .expect("auth recording on");
    assert!(
        auth_calls.is_empty(),
        "GET /v1/models never runs external auth, whatever transport carried the key"
    );
}
```

### T6 — 文档同步

| 文件 | 位置 | 改成 |
|---|---|---|
| `dev-docs/design.md` | 611 行 §6.3 第 3 步 | "解析客户端 api-key：接受 `Authorization: Bearer <k>`、`Authorization: <k>`（裸值）、`x-api-key`、`api-key`、`x-goog-api-key`、query `key/api_key/apikey/access_token`（优先级即此顺序；多来源冲突取最高优先级并告警；详见证 dev-docs/aegis/plans/2026-09-10-inbound-credential-forms.md）" |
| `dev-docs/design.md` | 655 行 §6.5 | "移除客户端原始 `Authorization`/`x-api-key`/`api-key`/`x-goog-api-key`" |
| `dev-docs/design.md` | 742 行 §7.1b | "客户端 api-key（任意受支持传输解析出的**原始值**）" |
| `crates/hydra-server/src/proxy/ctx.rs` | `client_api_key` 字段注释 | "parsed from any supported credential transport (see `hydra_core::apikey`)" |
| `crates/hydra-server/src/proxy.rs` | 模块头 15 行附近 | 补一句：缺 key 仍 401 `missing_api_key`，但接受多种传输 |
| `dev-docs/ops.md` | 495 行排障 | 补一句：若 Gemini/Azure 风格头仍 401，检查中间层是否剥离了 `x-goog-api-key`/`api-key` |
| `dev-docs/aegis/INDEX.md` | 表尾 | 追加本计划行 |

### T7 — 全量验证与提交

```bash
cargo test -p hydra-core
cargo test -p hydra-server --features server
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo build --workspace --features hydra-server/server
```

预期：全绿；`hydra-core` 单测 **+14**，`hydra-server` 集成 **+6**（全部为完整用例，无占位）。

提交切分（一个连贯 Task 一个提交）：
1. `feat(core): add client credential transport parser`（T1–T3）
2. `feat(proxy): accept client api-key from every HTTP transport`（T4–T5）
3. `docs: document inbound credential transports`（T6）

---

## 7. 测试矩阵

| 契约 | 单元测试（core） | 集成测试（server） |
|---|---|---|
| C1 优先级 | `bearer_wins_over_every_later_transport`、`precedence_x_api_key_over_api_key_over_google`、`query_names_are_searched_in_order` | `every_header_credential_transport_authenticates`、`query_credential_transport_authenticates_and_is_not_forwarded` |
| C1 大小写 / 裸值 | `bearer_scheme_is_case_insensitive`、`bare_authorization_value_is_a_key` | `authorization bare` 用例 |
| C2 冲突 | `identical_values_are_not_conflicts`、`falls_back_to_query_only_when_no_header_carried_a_key` | `differing_transports_use_the_highest_precedence_value` |
| C3 缺失 | `no_credentials_anywhere_is_empty`、`query_skips_empty_values_and_keeps_searching` | `no_credential_in_any_transport_is_401_missing_api_key` |
| C4 规范化 | `header_values_are_trimmed_and_empties_skipped`、`valueless_bearer_falls_through`、`query_values_are_percent_decoded_but_plus_is_literal` | — |
| C1/C4 反例 | `non_bearer_scheme_is_not_a_key_and_falls_through` | `non_bearer_authorization_scheme_is_rejected` |
| C5 下游不变 | — | `differing_transports…`（auth 收到的 key）、`catalog_narrowing_still_works_through_the_query_transport` |
| C6 安全 | — | query 不转发上游断言 |
| 日志标签 | `source_labels_are_stable_and_value_free` | — |

---

## 8. 风险与回滚

| 风险 | 影响 | 处置 |
|---|---|---|
| 新增来源改变现有客户端解析结果 | 低：C1 前两位覆盖现有两种来源，且 Bearer 仍最高优先 | 既有 `terminate_mode.rs` / `http_auth.rs` 全量回归必须绿 |
| query 形式的 key 泄漏到日志/Referer | 中：Hydra 自身不记录 URI（B4），但前置代理/CDN 可能记录 | C6 两条断言锁死网关侧；运维侧建议关闭前置代理的 query 日志（写入 ops.md） |
| `Basic` 等 scheme 被误当 key | 低：明确不解析（单测 + 集成负向 pin 各一） | — |
| 集群模式（edge）行为不一致 | 无：数据面不转发、edge 自持快照，解析在本地完成 | B6 已核实 |
| 回滚 | — | 纯增量：回滚 `apikey.rs` + `proxy.rs` 两处即恢复原状；无 schema/数据变更 |

**兼容性边界**：对外只增不减 —— 原两种形式行为完全不变（含 `Bearer ` 空值下探、trim 语义、401 响应体形状）；`auth_url` 请求体里的 `api_key` 字段语义不变。

**退役**：无退役项。旧的 40 行 `extract_api_key` 被整体替换（不是并存），不存在双解析路径。

---

## 9. 明确不做（建议后续独立立项）

1. **上游注入形式**——`provider_client.rs:112` 硬编码 `Authorization: Bearer`：真 Anthropic 需要 `x-api-key`、Gemini 需要 `x-goog-api-key`；且只转发 `Accept`/`Content-Type`（`provider_client.rs:124-133`），`anthropic-version` 等必需头会丢失。**这是比入口更严重的实际缺口**，需要动 `Provider` 实体 + migration + admin API + UI。
2. query 中**其它**参数向上游透传（当前一律丢弃，例如 Azure 的 `api-version`）。
3. Cookie / SigV4 / OAuth 动态取 token / mTLS 客户端证书。
4. 租户级形式白名单（本次按 D3 明确不做）。

---

## 10. 实施记录（2026-09-10）

**状态：已完成并验证（T1–T7 全部落地）。**

| 项 | 结果 |
|---|---|
| `crates/hydra-core/src/apikey.rs` | 新建，409 行（解析器 + 14 个单测） |
| `crates/hydra-core/src/lib.rs` | `+pub mod apikey;`（依赖白名单未变，compile_gate 通过） |
| `crates/hydra-server/src/proxy.rs` | import / `extract_api_key` / 调用点三处；`+67/-29` |
| `crates/hydra-server/src/proxy/ctx.rs` | `client_api_key` 注释同步 |
| `crates/hydra-server/tests/terminate_mode.rs` | `+339` 行：2 helper + 6 用例 |
| `dev-docs/design.md` §6.3/§6.5/§7.1b、`dev-docs/ops.md` §10.7 | 契约同步 |
| `cargo test -p hydra-core` | **141 passed / 0 failed**（含新增 14） |
| `cargo test -p hydra-server --features server` | **258 passed / 0 failed**（terminate_mode 32，含新增 6） |
| `cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings` | 干净 |
| `cargo build --workspace --features hydra-server/server` | 成功 |

**执行期修正**：写 T1 代码时发现 `Authorization: Bearer`（无值）trim 后不含空白、会被误判为裸 key —— 已在 `parse_authorization` 增加 `None if value.eq_ignore_ascii_case("bearer")` 分支，并由 `valueless_bearer_falls_through` 覆盖。

**遗留（未做，见 §9）**：上游注入形式仍是 `Authorization: Bearer` 硬编码；`?api-version` 等 query 参数仍不向上游透传。

---

## 11. 后续变更：上游凭据按路径选择（2026-09-10，同日实施）

**背景**：§9 遗留项 1（上游注入硬编码 `Authorization: Bearer`）已实施修复。

**决策**：用户最初要求"上游同时传 OpenAI 与 Anthropic 两种方式"，但检索到反证后改为**按路径选一种**：

- QwenLM/qwen-code [PR #4385](https://github.com/QwenLM/qwen-code/pull/4385) 回滚了完全相同的双发改动，理由 *regresses IdeaLab-style proxies*；
- `0x6d61/wn-core` Issue #46：SDK 同时发 `x-api-key` 导致 401；
- 反方向（OmniRoute PR #4729 给 anthropic 兼容网关补发 Bearer）说明两类网关都存在，而 Hydra 的 provider 只配 endpoint、无法预知对面类别。

故采用与项目既有"路径决定格式"惯例一致的单凭据规则。

**改动**

| 文件 | 内容 |
|---|---|
| `crates/hydra-core/src/model.rs` | 新增 `protocol_for_path(path) -> ProviderKind`（唯一owner）+ 1 个单测；proxy 的 usage-scanner 选型改为复用它，消除重复判定 |
| `crates/hydra-server/src/proxy/provider_client.rs` | 凭据注入按 `protocol_for_path` 分派：Anthropic 路径 → `x-api-key` + 透传 `anthropic-version`/`anthropic-beta`；其余 → `Authorization: Bearer`；+2 个单测（含"绝不双发"断言） |
| `crates/hydra-server/tests/anthropic_passthrough.rs` | +1 端到端用例：上游收到 `x-api-key: sk-upstream-secret`、无 `Authorization`、`anthropic-version` 存活 |
| `dev-docs/design.md` §6.5 | 契约同步（含"绝不双发"的理由） |

**验证（实跑）**：`cargo test -p hydra-core` → **142 passed / 0 failed**；`cargo test -p hydra-server --features server` → **261 passed / 0 failed**（`provider_client` 单测 4、`anthropic_passthrough` 3、`terminate_mode` 32）；`clippy --all-targets -D warnings` 干净；workspace build 成功。

**已知边界**（有意保留，未扩大）：判定只匹配 `/v1/messages` 结尾 —— `/v1/messages/count_tokens`、`/v1/messages/batches` 仍走 OpenAI 兼容分支（与改动前的 usage-scanner 边界完全一致，无回归）。需要时改一行谓词即可扩展，但会同时改变这两个端点的 usage 解析族，故未在本次动。
