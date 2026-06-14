//! Single-binary UI serving (the `embed-ui` feature). The built `web/dist` is
//! embedded at compile time; a fallback handler serves static assets and, for
//! unknown non-API paths, `index.html` (so client-side routing works). Unknown
//! `/api` paths still return the JSON 404, never the SPA shell.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct Assets;

/// Router fallback: serve an embedded asset, else the SPA shell, else 404.
pub(crate) async fn fallback(uri: Uri) -> Response {
    if uri.path().starts_with("/api") {
        return epiphany_api::ApiError::not_found("no such route").into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if let Some(asset) = Assets::get(path) {
        return serve(path, asset.data.into_owned());
    }
    match Assets::get("index.html") {
        Some(asset) => serve("index.html", asset.data.into_owned()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn serve(path: &str, body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, content_type(path))], body).into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
