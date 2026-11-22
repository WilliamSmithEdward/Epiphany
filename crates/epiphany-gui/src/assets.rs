//! The application's files: built into the program, or read from a folder while it is being
//! worked on.

use std::hash::{DefaultHasher, Hash, Hasher};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::{Gui, refusal};

/// The files, by the path they are served at.
macro_rules! files {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../web/", $path)))),*]
    };
}

const FILES: &[(&str, &[u8])] = files![
    "index.html",
    "styles.css",
    "icon.svg",
    "app.js",
    "api.js",
    "ui.js",
    "explore.js",
    "chart.js",
    "model.js",
    "admin.js",
    "sets.js",
    "selection.js",
    "tree.js",
    "later.js",
];

/// What a file is, by its extension.
fn kind(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// A file, or the application's page for any address it routes itself: one with no
/// extension.
pub(crate) async fn serve(State(gui): State<Gui>, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = match path.is_empty() || !path.rsplit('/').next().unwrap_or("").contains('.') {
        true => "index.html",
        false => path,
    };
    let body = match &gui.0.web {
        // Read afresh, and only from inside the folder.
        Some(folder) if !path.split('/').any(|part| part == ".." || part.is_empty()) => {
            std::fs::read(folder.join(path)).ok()
        }
        Some(_) => None,
        None => FILES
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, body)| body.to_vec()),
    };
    let Some(body) = body else {
        return refusal(StatusCode::NOT_FOUND, "not_found", "there is no such file");
    };
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    let tag = format!("\"{:016x}\"", hasher.finish());
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|seen| seen == tag.as_str());
    let tag = HeaderValue::from_str(&tag).expect("hex digits");
    // Always asked about again, and sent again only when it has changed.
    let cache = HeaderValue::from_static("no-cache");
    match fresh {
        true => (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, tag), (header::CACHE_CONTROL, cache)],
        )
            .into_response(),
        false => {
            let kind = HeaderValue::from_static(kind(path));
            (
                [
                    (header::CONTENT_TYPE, kind),
                    (header::ETAG, tag),
                    (header::CACHE_CONTROL, cache),
                ],
                body,
            )
                .into_response()
        }
    }
}
