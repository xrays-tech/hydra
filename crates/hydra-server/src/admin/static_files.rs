//! Embedded static UI for `/admin/*` (design §14.2).
//!
//! The UI assets (`index.html`, `app.js`, `api-docs.js`, `style.css`) live under
//! `admin-ui/` at the workspace root and are baked into the binary at compile
//! time via `include_dir!`. There is **no npm/build step**: the files are plain
//! static HTML/CSS/JS that the browser loads as-is.
//!
//! ## Why the assets bypass the admin token gate
//!
//! The static HTML/CSS/JS contain no secrets — the admin bearer token is what
//! is sensitive. The UI therefore loads *unauthenticated* (so the browser can
//! render the login prompt), and `app.js` then asks for the token and attaches
//! `Authorization: Bearer <token>` to every subsequent `fetch('/api/v1/*')`.
//! All `/api/v1/*` and `/metrics` routes remain token-gated as in W5.
//!
//! ## Embedding path
//!
//! `include_dir!` requires a literal path relative to the crate directory. The
//! UI lives at `<workspace>/admin-ui/`, i.e. two levels up from this crate, so
//! the path is `$CARGO_MANIFEST_DIR/../../admin-ui`.

use std::sync::OnceLock;

use http::Response;
use include_dir::{include_dir, Dir};

use super::handlers::Resp;

/// The embedded `admin-ui/` directory (compiled into the binary).
static ADMIN_UI: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../admin-ui");

/// Index document served at `/admin/` and `/admin` (design §14.2 T1.1).
const INDEX_DOC: &str = "index.html";

/// All files embedded in `admin-ui/` (computed once on first access).
fn ui_files() -> &'static [(&'static str, &'static [u8])] {
    static CACHE: OnceLock<Vec<(&'static str, &'static [u8])>> = OnceLock::new();
    CACHE.get_or_init(|| {
        ADMIN_UI
            .files()
            .map(|f| {
                (
                    f.path()
                        .file_name()
                        .unwrap_or_default()
                        .to_str()
                        .unwrap_or(""),
                    f.contents(),
                )
            })
            .collect()
    })
}

/// Serve `/admin/*` (and the index at `/admin/` or `/admin`).
///
/// `path` is the full request path. Returns `None` when it is not a `/admin`
/// route (so the caller can fall through to the API router); `Some(resp)` when
/// it is, including a 404 for unknown assets under `/admin/`.
pub(super) fn try_serve_admin(path: &str) -> Option<Resp> {
    // Normalise: `/admin`, `/admin/`, `/admin/index.html` all → index;
    // `/admin/<asset>` → asset.
    let rest = if path == "/admin" {
        ""
    } else {
        path.strip_prefix("/admin/")?
    };
    let name = if rest.is_empty() || rest == INDEX_DOC {
        INDEX_DOC
    } else {
        rest
    };
    // Reject anything that looks like path traversal or has a sub-path — only
    // top-level files in `admin-ui/` are served.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Some(text_response(404, "text/plain", b"not found"));
    }
    let file = ui_files().iter().find(|(n, _)| *n == name);
    let (bytes, ctype) = match file {
        Some((_, bytes)) => (*bytes, content_type_for(name)),
        None => return Some(text_response(404, "text/plain", b"not found")),
    };
    Some(bytes_response(200, ctype, bytes))
}

/// Map a file extension to a Content-Type for the small set the UI ships.
fn content_type_for(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

fn bytes_response(status: u16, content_type: &str, body: &[u8]) -> Resp {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("content-length", body.len().to_string())
        // Same-origin: the UI fetches /api/v1/* and /admin/* from one origin.
        // No CORS headers are needed and none are emitted.
        .body(body.to_vec())
        .unwrap_or_else(|_| Response::new(vec![]))
}

fn text_response(status: u16, content_type: &str, body: &[u8]) -> Resp {
    bytes_response(status, content_type, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_served_with_html_content_type() {
        let r = try_serve_admin("/admin/").expect("index");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "got {ct}");
        let body = String::from_utf8(r.body().to_vec()).unwrap();
        assert!(body.contains("<title>Hydra Admin</title>"));
    }

    #[test]
    fn admin_without_slash_serves_index() {
        let r = try_serve_admin("/admin").expect("index");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "got {ct}");
    }

    #[test]
    fn app_js_served_with_js_content_type() {
        let r = try_serve_admin("/admin/app.js").expect("app.js");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("application/javascript"), "got {ct}");
        let body = String::from_utf8(r.body().to_vec()).unwrap();
        // A sentinel from the source so the embedded file is provably correct.
        assert!(body.contains("Hydra admin UI"));
    }

    #[test]
    fn style_css_served_with_css_content_type() {
        let r = try_serve_admin("/admin/style.css").expect("style.css");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/css"), "got {ct}");
    }

    #[test]
    fn index_doc_alias_serves_index() {
        let r = try_serve_admin("/admin/index.html").expect("index");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "got {ct}");
    }

    #[test]
    fn unknown_asset_returns_404_under_admin_prefix() {
        // Unknown asset under /admin/* → 404 (still "handled" → Some).
        let r = try_serve_admin("/admin/nope.xyz").expect("handled");
        assert_eq!(r.status().as_u16(), 404);
    }

    #[test]
    fn path_traversal_rejected() {
        let r = try_serve_admin("/admin/..%2fetc/passwd").expect("handled");
        // The `..` substring in the request path is decoded by the client; the
        // router here sees a literal ".." and rejects it.
        assert_eq!(r.status().as_u16(), 404);
    }

    #[test]
    fn non_admin_path_not_handled() {
        assert!(try_serve_admin("/api/v1/health").is_none());
        assert!(try_serve_admin("/metrics").is_none());
        assert!(try_serve_admin("/").is_none());
    }

    #[test]
    fn index_html_loads_api_docs_before_app_js() {
        // Regression: api-docs.js defines the global renderApiDocs() that
        // app.js's top level reads eagerly (CUSTOM["api-docs"].render). Classic
        // <script> tags run in document order, so loading api-docs.js AFTER
        // app.js raised "Uncaught ReferenceError: renderApiDocs is not defined"
        // on the /admin page and aborted the whole UI.
        let r = try_serve_admin("/admin/").expect("index");
        let body = String::from_utf8(r.body().to_vec()).unwrap();
        let app_pos = body
            .find("<script src=\"/admin/app.js\">")
            .unwrap_or_else(|| panic!("index.html must load app.js"));
        let docs_pos = body
            .find("<script src=\"/admin/api-docs.js\">")
            .unwrap_or_else(|| panic!("index.html must load api-docs.js"));
        // Same eager-read pattern for the stats page (CUSTOM["stats"].render =
        // renderStats defined in stats.js).
        let stats_pos = body
            .find("<script src=\"/admin/stats.js\">")
            .unwrap_or_else(|| panic!("index.html must load stats.js"));
        // i18n.js defines the globals t()/setLang()/applyStaticI18n() that
        // every other script uses at load/render time, so it must come FIRST.
        let i18n_pos = body
            .find("<script src=\"/admin/i18n.js\">")
            .unwrap_or_else(|| panic!("index.html must load i18n.js"));
        assert!(
            i18n_pos < docs_pos && i18n_pos < stats_pos && i18n_pos < app_pos,
            "i18n.js must load before api-docs.js / stats.js / app.js"
        );
        assert!(
            docs_pos < app_pos && stats_pos < app_pos,
            "api-docs.js and stats.js must load before app.js (app.js top level reads renderApiDocs / renderStats)"
        );
    }

    #[test]
    fn i18n_js_served_with_four_locales() {
        let r = try_serve_admin("/admin/i18n.js").expect("i18n.js");
        assert_eq!(r.status().as_u16(), 200);
        let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("application/javascript"), "got {ct}");
        let body = String::from_utf8(r.body().to_vec()).unwrap();
        assert!(
            body.contains("I18N"),
            "i18n.js must define the I18N dictionary"
        );
        for marker in ["\"zh\"", "\"fr\"", "\"de\"", "\"en\""] {
            assert!(body.contains(marker), "missing locale marker {marker}");
        }
    }
}
