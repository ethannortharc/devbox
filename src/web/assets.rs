//! Static assets, compiled into the binary.
//!
//! htmx, its SSE extension, and xterm.js are vendored under
//! `src/web/assets/` and embedded with [`rust_embed`]. Nothing is fetched from
//! a CDN at runtime — the console works with no network at all, which is what
//! keeps the "single binary" promise honest.
//! A product rebuild snapshots the lifecycle/SSE client code into that binary.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/web/assets/"]
pub struct Assets;

/// Serve an embedded asset by path, e.g. `/assets/js/htmx.min.js`.
///
/// Content type is inferred from the extension. The response is validated
/// against its content hash rather than cached blind.
///
/// It used to be `immutable` for a year on a versionless URL, justified by "a
/// new binary means a new process means a new cache" — which is not true of the
/// party doing the caching. The browser's cache outlives the server, so an
/// upgraded devbox kept serving the *previous* build's scripts, indefinitely
/// and silently. That was survivable while the assets were only cosmetic. It
/// stopped being survivable when `htmx-config.js` became the thing that
/// attaches the console key: a stale copy authenticates nothing, so the upgrade
/// would have presented every user with a console where every action returned
/// 401 and a hard reload was the only cure.
///
/// A version in the query string would fix that once and then need remembering
/// at every future asset reference. The hash needs remembering nowhere.
pub async fn handler(headers: HeaderMap, Path(path): Path<String>) -> Response {
    let Some(file) = Assets::get(path.as_str()) else {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    };

    let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()));
    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|candidates| {
            // A validator list, not a single value: browsers may send several,
            // and `*` matches whatever we hold.
            candidates
                .split(',')
                .any(|c| c.trim() == etag || c.trim() == "*")
        });

    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    // `no-cache` is "revalidate before use", not "do not store" — the body is
    // still cached, and an unchanged asset costs a 304 with no payload.
    let meta = [
        (header::CONTENT_TYPE, mime.as_ref()),
        (header::CACHE_CONTROL, "no-cache"),
        (header::ETAG, etag.as_str()),
    ];

    if known {
        return (StatusCode::NOT_MODIFIED, meta).into_response();
    }
    (StatusCode::OK, meta, Body::from(file.data.into_owned())).into_response()
}

/// Lowercase hex, for the ETag.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `GET /favicon.ico` — browsers ask for this whether or not the page links
/// it. Serve the embedded SVG (every current browser accepts an SVG here).
pub async fn favicon(headers: HeaderMap) -> Response {
    handler(headers, Path("favicon.svg".to_string())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn favicon_is_served() {
        let res = favicon(HeaderMap::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/svg+xml"
        );
    }

    #[tokio::test]
    async fn an_asset_revalidates_rather_than_being_cached_blind() {
        // The upgrade path depends on this. `htmx-config.js` is what attaches
        // the console key, so a browser that never re-asks for it keeps a copy
        // that authenticates nothing.
        let res = handler(HeaderMap::new(), Path("js/htmx-config.js".into())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );

        let etag = res.headers().get(header::ETAG).unwrap().clone();
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.clone());
        let again = handler(headers, Path("js/htmx-config.js".into())).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);

        // And a stale validator gets the new body rather than a 304.
        let mut stale = HeaderMap::new();
        stale.insert(header::IF_NONE_MATCH, "\"nope\"".parse().unwrap());
        let refetched = handler(stale, Path("js/htmx-config.js".into())).await;
        assert_eq!(refetched.status(), StatusCode::OK);
    }

    #[test]
    fn distinct_assets_get_distinct_validators() {
        let a = Assets::get("js/key.js").unwrap();
        let b = Assets::get("js/shell.js").unwrap();
        assert_ne!(
            hex(&a.metadata.sha256_hash()),
            hex(&b.metadata.sha256_hash())
        );
    }

    #[test]
    fn vendored_assets_are_embedded() {
        for path in [
            "js/htmx.min.js",
            "js/sse.js",
            "js/xterm.js",
            "js/xterm-addon-fit.js",
            // The console key's whole client side. `key.js` defines it,
            // `bootstrap.js` installs it, `shell.js` presents it on the first
            // hop; a missing one is a console that cannot authenticate at all.
            "js/key.js",
            "js/bootstrap.js",
            "js/shell.js",
            "js/htmx-config.js",
            "js/term.js",
            "css/app.css",
            "css/xterm.css",
        ] {
            let file = Assets::get(path);
            assert!(file.is_some(), "missing embedded asset: {path}");
            assert!(
                !file.unwrap().data.is_empty(),
                "embedded asset is empty: {path}"
            );
        }
    }

    #[test]
    fn dashboard_sse_merges_around_lifecycle_requests() {
        let file = Assets::get("js/htmx-config.js").unwrap();
        let source = std::str::from_utf8(&file.data).unwrap();

        assert!(source.contains("mergeBoxesSnapshot(e.detail.data"));
        assert!(source.contains("type === 'boxes' && document.getElementById('box-grid')"));
        assert!(source.contains("current.matches('.htmx-request')"));
        assert!(source.contains("current.querySelector('.htmx-request')"));
        assert!(source.contains("current.replaceWith(replacement)"));
        assert!(source.contains("mergeDetailSnapshotPreservingError"));
        assert!(source.contains("preserveLifecycleErrorForSameStatus(current, replacement)"));
        assert!(source.contains("current.dataset.boxStatus !== incoming.dataset.boxStatus"));
        assert!(source.contains("error.cloneNode(true)"));
    }

    #[tokio::test]
    async fn unknown_asset_is_404() {
        let res = handler(HeaderMap::new(), Path("js/nope.js".to_string())).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
