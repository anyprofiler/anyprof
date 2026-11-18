//! The viewer's static files, gzip-compressed by `build.rs`.
//!
//! `index.html`, `app.css`, and `app.js` are the whole UI. Profile names are
//! not interpolated into them.

/// One embedded file, stored gzip-compressed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Asset {
    /// Gzip-compressed bytes.
    pub gzip: &'static [u8],
    /// `Content-Type` for the uncompressed file.
    pub content_type: &'static str,
}

/// The embedded file called `name`, which must be a bare file name.
pub(crate) fn lookup(name: &str) -> Option<Asset> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let (gzip, content_type) = match name {
        "index.html" => (
            include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz")).as_slice(),
            "text/html; charset=utf-8",
        ),
        "app.css" => (
            include_bytes!(concat!(env!("OUT_DIR"), "/app.css.gz")).as_slice(),
            "text/css; charset=utf-8",
        ),
        "app.js" => (
            include_bytes!(concat!(env!("OUT_DIR"), "/app.js.gz")).as_slice(),
            "text/javascript; charset=utf-8",
        ),
        _ => return None,
    };
    Some(Asset { gzip, content_type })
}

#[cfg(test)]
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    ["index.html", "app.css", "app.js"].into_iter()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::{lookup, names};

    fn gunzip(bytes: &[u8]) -> String {
        let mut out = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn the_page_references_only_the_embedded_assets() {
        let index = gunzip(lookup("index.html").unwrap().gzip);
        assert!(index.contains("app.css"));
        assert!(index.contains("app.js"));
        assert!(!index.contains("speedscope"));
        assert!(!index.contains("http://"));
        assert!(!index.contains("https://"));
        for name in names() {
            assert!(lookup(name).is_some(), "{name}");
        }
    }

    #[test]
    fn lookup_only_accepts_bare_names() {
        assert!(lookup("index.html").is_some());
        assert!(lookup("app.js").is_some());
        assert!(lookup("app.css").is_some());
        for name in [
            "",
            "../index.html",
            "./index.html",
            "sub/index.html",
            "..\\index.html",
            "index.html.gz",
            ".",
            "..",
            "profile",
        ] {
            assert!(lookup(name).is_none(), "{name:?}");
        }
    }

    #[test]
    fn content_types_match_extensions() {
        assert_eq!(
            lookup("index.html").unwrap().content_type,
            "text/html; charset=utf-8"
        );
        assert_eq!(
            lookup("app.js").unwrap().content_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            lookup("app.css").unwrap().content_type,
            "text/css; charset=utf-8"
        );
    }
}
