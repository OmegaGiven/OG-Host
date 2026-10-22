//! The OG-Host web UI: static assets compiled into the binary (sign-in lives in auth.rs).
//!
//! Assets (crates/og-hostd/ui) are embedded, gzipped once at startup and served with ETags,
//! so the UI loads from the daemon itself with no CDN or internet access.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};

// ---- assets -------------------------------------------------------------------------------

struct Asset {
    mime: &'static str,
    raw: &'static [u8],
    gz: Vec<u8>,
    etag: String,
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        ($path, $mime, include_bytes!(concat!("../ui/", $path)) as &'static [u8])
    };
}

static ASSETS: LazyLock<HashMap<&'static str, Asset>> = LazyLock::new(|| {
    use std::io::Write;
    [
        asset!("index.html", "text/html; charset=utf-8"),
        asset!("app.css", "text/css; charset=utf-8"),
        asset!("app.js", "text/javascript; charset=utf-8"),
        asset!("lab.js", "text/javascript; charset=utf-8"),
        asset!("access.js", "text/javascript; charset=utf-8"),
        asset!("backups.js", "text/javascript; charset=utf-8"),
        asset!("favicon.svg", "image/svg+xml"),
    ]
    .into_iter()
    .map(|(name, mime, raw)| {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = enc.write_all(raw);
        let gz = enc.finish().unwrap_or_default();
        // FNV-1a: good enough to tell versions apart
        let h = raw.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3));
        (name, Asset { mime, raw, gz, etag: format!("\"{h:016x}\"") })
    })
    .collect()
});

pub fn security_headers(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

pub async fn asset(req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let name = if path.is_empty() || !path.contains('.') { "index.html" } else { path };
    // development: OG_HOST_UI_DIR serves the files from disk (set by root in the unit)
    if let Ok(dir) = std::env::var("OG_HOST_UI_DIR") {
        if let Some(a) = ASSETS.get(name) {
            if let Ok(body) = std::fs::read(std::path::Path::new(&dir).join(name)) {
                let mut r = Response::new(Body::from(body));
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(a.mime));
                r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                security_headers(&mut r);
                return r;
            }
        }
    }
    let Some(a) = ASSETS.get(name) else { return StatusCode::NOT_FOUND.into_response() };
    let inm = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let mut r = if inm == Some(a.etag.as_str()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let gzip = req.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
        let mut r = Response::new(Body::from(if gzip { a.gz.clone() } else { a.raw.to_vec() }));
        if gzip {
            r.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(a.mime));
        r
    };
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&a.etag).unwrap());
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r.headers_mut().insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    security_headers(&mut r);
    r
}

