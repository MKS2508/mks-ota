//! Custom URI scheme serving the partial artifact (extracted from
//! `tpv-el-haido2`'s production updater, `ota/protocol.rs`).
//!
//! The window ALWAYS loads from this scheme, whether a partial artifact is
//! installed or not. The webview's origin never changes: switching between
//! `tauri://` and this scheme on every swap would drop `localStorage`
//! (onboarding, theme, storage mode). Which side serves what is decided in
//! here.
//!
//! Requires the `tauri` feature.

use std::fs;
use std::path::Path;

use tauri::http;
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::install::partial::slots;

/// URL the main window loads from, for the app's scheme name.
///
/// Tauri uses the `WebviewUrl::CustomProtocol` value verbatim, without
/// adapting it per platform, and the scheme is not registered the same way
/// everywhere: on Windows and Android it lives under
/// `http://<scheme>.localhost`. That is why the window is built in Rust and
/// not from `tauri.conf.json`, which has no per-platform value.
pub fn window_url(scheme: &str) -> String {
    if cfg!(any(windows, target_os = "android")) {
        format!("http://{scheme}.localhost")
    } else {
        format!("{scheme}://localhost")
    }
}

/// Content-Type by extension.
///
/// Not decoration: a `.js` served with the wrong MIME makes the browser
/// reject the ES module and the app boots to a blank screen with no clear
/// error.
fn mime_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Extracts the requested resource path, without query or fragment.
fn request_path(uri: &str) -> String {
    let without_scheme = uri.split("://").nth(1).unwrap_or(uri);
    let path = without_scheme.find('/').map_or("/", |i| &without_scheme[i..]);
    let path = path.split(['?', '#']).next().unwrap_or("/");

    if path.is_empty() || path == "/" {
        "/index.html".to_string()
    } else {
        path.to_string()
    }
}

fn respond(responder: UriSchemeResponder, status: u16, mime: &str, body: Vec<u8>) {
    let response = http::Response::builder()
        .status(status)
        .header("Content-Type", mime)
        // The webview is the only client of this scheme; the artifact must
        // not be embeddable by anyone else.
        .header("X-Content-Type-Options", "nosniff")
        .body(body);

    match response {
        Ok(res) => responder.respond(res),
        Err(err) => eprintln!("[ota] could not build the response: {err}"),
    }
}

/// Scheme handler: serves from the active slot if there is one, otherwise
/// from the assets embedded in the binary.
///
/// The embedded copy is the safety net and is always available: any failure
/// reading the slot falls through to it instead of leaving the window blank.
pub fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let path = request_path(&request.uri().to_string());
    let app = ctx.app_handle();

    let from_slot = app.path().app_data_dir().ok().and_then(|data_dir| {
        let state = slots::load_state(&data_dir);
        let slot_dir = slots::active_dir(&data_dir, &state)?;
        let file = slots::resolve_within(&slot_dir, &path)?;
        match fs::read(&file) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                eprintln!("[ota] could not read {} from the slot: {err}", file.display());
                None
            }
        }
    });

    if let Some(bytes) = from_slot {
        respond(responder, 200, mime_for(&path), bytes);
        return;
    }

    // Embedded assets. `asset_resolver` expects the path without the
    // leading slash.
    let embedded_path = path.trim_start_matches('/').to_string();
    if let Some(asset) = app.asset_resolver().get(embedded_path) {
        respond(responder, 200, &asset.mime_type.clone(), asset.bytes);
        return;
    }

    respond(
        responder,
        404,
        "text/plain; charset=utf-8",
        format!("Not found: {path}").into_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_resolves_to_index() {
        assert_eq!(request_path("myapp://localhost/"), "/index.html");
        assert_eq!(request_path("myapp://localhost"), "/index.html");
        assert_eq!(request_path("http://myapp.localhost/"), "/index.html");
    }

    #[test]
    fn query_and_fragment_are_dropped() {
        assert_eq!(request_path("myapp://localhost/app.js?v=3"), "/app.js");
        assert_eq!(request_path("myapp://localhost/a.css#top"), "/a.css");
    }

    #[test]
    fn nested_paths_are_preserved() {
        assert_eq!(request_path("myapp://localhost/assets/index-a1b2.js"), "/assets/index-a1b2.js");
    }

    #[test]
    fn es_modules_get_their_mime() {
        // Serving JS as octet-stream leaves the app blank with no readable
        // error.
        assert!(mime_for("/assets/index.js").starts_with("text/javascript"));
        assert!(mime_for("/assets/index.mjs").starts_with("text/javascript"));
        assert!(mime_for("/index.html").starts_with("text/html"));
        assert!(mime_for("/a.css").starts_with("text/css"));
        assert_eq!(mime_for("/f.woff2"), "font/woff2");
        assert_eq!(mime_for("/x.unknown"), "application/octet-stream");
    }

    #[test]
    fn the_mime_does_not_depend_on_case() {
        assert!(mime_for("/LOGO.PNG").starts_with("image/png"));
    }
}
