//! The stylesheet, script and icon, embedded in the binary.
//!
//! Pages link them with `?v=<content hash>`; a request carrying the current
//! hash may be cached for a year, any other request must revalidate.

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

const CSS: &str = include_str!("../../assets/app.css");
const JS: &str = include_str!("../../assets/app.js");
const ICON: &str = include_str!("../../assets/icon.svg");

/// FNV-1a: only a cache-busting fingerprint, not a security measure.
const fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}

const CSS_HASH: u64 = fingerprint(CSS.as_bytes());
const JS_HASH: u64 = fingerprint(JS.as_bytes());
const ICON_HASH: u64 = fingerprint(ICON.as_bytes());

pub fn css_url() -> String {
    format!("/assets/app.css?v={CSS_HASH:016x}")
}

pub fn js_url() -> String {
    format!("/assets/app.js?v={JS_HASH:016x}")
}

pub fn icon_url() -> String {
    format!("/assets/icon.svg?v={ICON_HASH:016x}")
}

#[derive(Deserialize)]
pub struct AssetQuery {
    v: Option<String>,
}

pub async fn serve_asset(
    Path(name): Path<String>,
    Query(query): Query<AssetQuery>,
    headers: HeaderMap,
) -> Response {
    let (body, content_type, hash) = match name.as_str() {
        "app.css" => (CSS, "text/css; charset=utf-8", CSS_HASH),
        "app.js" => (JS, "text/javascript; charset=utf-8", JS_HASH),
        "icon.svg" => (ICON, "image/svg+xml", ICON_HASH),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let etag = format!("\"{hash:016x}\"");
    let current = query.v.as_deref() == Some(format!("{hash:016x}").as_str());
    let cache = if current {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, content_type)], body).into_response()
    };
    let map = response.headers_mut();
    map.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        map.insert(header::ETAG, value);
    }
    response
}
