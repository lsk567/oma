//! The Mission Control bundle `omar serve` hands out.
//!
//! Served from the daemon's own port, so the page is same-origin with the API
//! it calls: no CORS, and the address is read from the page rather than chosen
//! at launch. That is the whole reason this exists rather than pointing a
//! separately-started dev server at the daemon.
//!
//! The bytes are stored gzipped and handed out gzipped, which is what keeps the
//! binary about 0.7MB larger rather than 2.2MB. Every browser has understood
//! `Content-Encoding: gzip` for twenty years, and this only ever answers
//! loopback, so there is no client here that cannot read it.
//!
//! Built by `npm run build:spa` in `web/`, embedded only under the `ui`
//! feature: a plain `cargo build` needs no Node.

/// A file the daemon will hand out, already compressed.
pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub gzipped: &'static [u8],
}

#[cfg(feature = "ui")]
mod embedded {
    use super::Asset;

    macro_rules! asset {
        ($path:expr, $content_type:expr, $file:expr) => {
            Asset {
                path: $path,
                content_type: $content_type,
                gzipped: include_bytes!(concat!("../web/dist/spa/", $file, ".gz")),
            }
        };
    }

    // The same five files `web/build/compress-spa.mjs` writes. Naming them on
    // both sides means a bundle that stopped emitting one fails to compile,
    // rather than 404ing in a browser.
    pub const ASSETS: &[Asset] = &[
        asset!("/", "text/html; charset=utf-8", "index.html"),
        asset!("/app.js", "text/javascript; charset=utf-8", "app.js"),
        asset!("/app.css", "text/css; charset=utf-8", "app.css"),
        asset!("/omar-logo.png", "image/png", "omar-logo.png"),
        asset!("/favicon.svg", "image/svg+xml", "favicon.svg"),
    ];
}

#[cfg(not(feature = "ui"))]
mod embedded {
    use super::Asset;

    pub const ASSETS: &[Asset] = &[];
}

pub use embedded::ASSETS;

/// Whether this binary was built with the UI in it.
pub fn is_bundled() -> bool {
    !ASSETS.is_empty()
}

/// The asset a request path asks for, if any.
pub fn lookup(path: &str) -> Option<&'static Asset> {
    // A single-page application: the client owns its routes, so anything that
    // is not a file it shipped is that shell again. `/v1` never reaches here.
    let wanted = if ASSETS.iter().any(|asset| asset.path == path) {
        path
    } else {
        "/"
    };
    ASSETS.iter().find(|asset| asset.path == wanted)
}

/// What to tell someone whose binary has no UI in it.
pub const MISSING: &str = "This build has no UI in it. Build one with:\n  \
    (cd web && npm install && npm run build:spa)\n  \
    cargo build --release --features ui\n\
    Or run `make dev`, which starts Mission Control from source.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_feature_says_so_rather_than_serving_nothing() {
        // Both halves have to agree: `is_bundled` is what decides whether
        // `--ui` opens a browser or explains itself.
        assert_eq!(is_bundled(), !ASSETS.is_empty());
        assert!(MISSING.contains("--features ui"));
    }

    #[cfg(feature = "ui")]
    #[test]
    fn every_embedded_asset_is_gzip_and_reachable() {
        for asset in ASSETS {
            assert!(!asset.gzipped.is_empty(), "{} is empty", asset.path);
            // gzip's magic number, so a raw file embedded by mistake fails here
            // rather than in a browser that cannot decode it.
            assert_eq!(
                &asset.gzipped[..2],
                &[0x1f, 0x8b],
                "{} is not gzip",
                asset.path
            );
            assert!(lookup(asset.path).is_some(), "{} not routable", asset.path);
        }
    }

    #[cfg(feature = "ui")]
    #[test]
    fn an_unknown_path_falls_back_to_the_shell() {
        // The client owns its routes; a reload on one must not 404.
        assert_eq!(lookup("/anything").map(|a| a.path), Some("/"));
        assert_eq!(lookup("/app.js").map(|a| a.path), Some("/app.js"));
    }
}
