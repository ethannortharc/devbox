//! Static assets, compiled into the binary.
//!
//! htmx, its SSE extension, and xterm.js are vendored under
//! `src/web/assets/` and embedded with [`rust_embed`]. Nothing is fetched from
//! a CDN at runtime — the console works with no network at all, which is what
//! keeps the "single binary" promise honest.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/web/assets/"]
pub struct Assets;

/// Serve an embedded asset by path, e.g. `/assets/js/htmx.min.js`.
///
/// Content type is inferred from the extension. Assets are immutable for the
/// life of a binary, so they are cached aggressively; the response is
/// versionless because a new binary means a new process means a new cache.
pub async fn handler(Path(path): Path<String>) -> Response {
    match Assets::get(path.as_str()) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                ],
                Body::from(file.data.into_owned()),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

/// `GET /favicon.ico` — browsers ask for this whether or not the page links
/// it. Serve the embedded SVG (every current browser accepts an SVG here).
pub async fn favicon() -> Response {
    handler(Path("favicon.svg".to_string())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn favicon_is_served() {
        let res = favicon().await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/svg+xml"
        );
    }

    #[test]
    fn vendored_assets_are_embedded() {
        for path in [
            "js/htmx.min.js",
            "js/sse.js",
            "js/xterm.js",
            "js/xterm-addon-fit.js",
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

    #[tokio::test]
    async fn unknown_asset_is_404() {
        let res = handler(Path("js/nope.js".to_string())).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
