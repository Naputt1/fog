//! Static SPA asset serving: fallback shell plus embedded asset bytes with
//! `Accept-Encoding` (brotli/gzip/identity) negotiation.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::{EmbeddedAsset, RespBody, api_error, embedded_get};

/// Serves a non-API request from the embedded SPA. Returns an exact static
/// asset when one exists (correct content-type), otherwise the SPA `index.html`
/// so client-side routes resolve. Returns 404 when no SPA is embedded.
///
/// `accept_encoding` is the request's `Accept-Encoding` header value; when the
/// client advertises `br`/`gzip` and the asset embeds that variant, it is
/// served with the matching `Content-Encoding` (brotli preferred). Hashed
/// `/assets/*` files (content-hash filenames from Vite) are immutable and
/// cached for a year; everything else (notably `index.html`) is `no-cache` so
/// deploys are picked up on reload.
pub(super) fn serve_spa_fallback(path: &str, accept_encoding: &str) -> Response<RespBody> {
    if let Some(asset) = embedded_get(path) {
        return asset_response(asset, path, accept_encoding);
    }
    // Serve the SPA entry point for any client-side route (and for `/`, which
    // the generated module also maps to `index.html`).
    if let Some(asset) = embedded_get("/index.html") {
        return asset_response(asset, "/index.html", accept_encoding);
    }
    api_error(StatusCode::NOT_FOUND, "page not found")
}

/// Serves a static asset body from embedded bytes with its content-type,
/// negotiating a precompressed encoding when the client supports one.
pub(super) fn asset_response(
    asset: EmbeddedAsset,
    url_path: &str,
    accept_encoding: &str,
) -> Response<RespBody> {
    // Brotli compresses JS/CSS ~15-20% better than gzip, so prefer it when
    // offered. Fall back to gzip, then identity.
    let (data, encoding) = if accepts_encoding(accept_encoding, "br") {
        if let Some(br) = asset.br {
            (br, Some("br"))
        } else if let Some(gzip) = asset
            .gzip
            .filter(|_| accepts_encoding(accept_encoding, "gzip"))
        {
            (gzip, Some("gzip"))
        } else {
            (asset.data, None)
        }
    } else if let Some(gzip) = asset
        .gzip
        .filter(|_| accepts_encoding(accept_encoding, "gzip"))
    {
        (gzip, Some("gzip"))
    } else {
        (asset.data, None)
    };
    // Vite emits content-hashed filenames under `/assets/` (e.g.
    // `index-CxVdPbko.js`), so those bytes are immutable. Entry HTML and
    // root-level files are unhashed and must revalidate.
    let cache_control = if url_path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", asset.mime)
        .header("cache-control", cache_control)
        .header("content-length", data.len());
    if asset.gzip.is_some() || asset.br.is_some() {
        builder = builder.header("vary", "accept-encoding");
    }
    if let Some(encoding) = encoding {
        builder = builder.header("content-encoding", encoding);
    }
    builder
        .body(Full::new(Bytes::from_static(data)).boxed())
        .expect("response builder failed")
}

/// Whether an `Accept-Encoding` header value offers `coding` with a nonzero
/// weight. Handles `br`, `br;q=0.8` and `*;q=0.5`; an explicit `q=0` refusal
/// (e.g. `gzip;q=0`) returns `false` even when `*` is present.
pub(super) fn accepts_encoding(header: &str, coding: &str) -> bool {
    let mut offered = false;
    let mut star = false;
    for part in header.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (token, params) = match part.split_once(';') {
            Some((token, params)) => (token.trim(), params),
            None => (part, ""),
        };
        // `q=0` (or `q=0.0…`) means "not acceptable".
        let refused = params.split(';').any(|param| {
            let param = param.trim();
            let Some(q) = param
                .strip_prefix("q=")
                .or_else(|| param.strip_prefix("Q="))
            else {
                return false;
            };
            q.trim().parse::<f32>().is_ok_and(|weight| weight == 0.0)
        });
        if token.eq_ignore_ascii_case(coding) {
            if refused {
                return false;
            }
            offered = true;
        } else if token == "*" && !refused {
            star = true;
        }
    }
    offered || star
}
