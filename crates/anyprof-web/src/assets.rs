use include_dir::{Dir, include_dir};

/// speedscope's static build, one `<name>.gz` per file. See
/// `scripts/vendor-speedscope.sh`.
static SPEEDSCOPE: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/vendor/speedscope");

/// One embedded file, stored gzip-compressed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Asset {
    pub gzip: &'static [u8],
    pub content_type: &'static str,
}

/// The embedded file called `name`, which must be a bare file name.
pub(crate) fn lookup(name: &str) -> Option<Asset> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let file = SPEEDSCOPE.get_file(format!("{name}.gz"))?;
    Some(Asset {
        gzip: file.contents(),
        content_type: content_type(name),
    })
}

#[cfg(test)]
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    SPEEDSCOPE
        .files()
        .filter_map(|file| file.path().to_str()?.strip_suffix(".gz"))
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
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
    fn every_file_index_and_css_reference_is_embedded() {
        let index = gunzip(lookup("index.html").unwrap().gzip);
        let css_name = names().find(|name| name.ends_with(".css")).unwrap();
        let css = gunzip(lookup(css_name).unwrap().gzip);
        let mut referenced = Vec::new();
        for source in [&index, &css] {
            for piece in source.split(['"', '(', ')']) {
                let piece = piece.trim_start_matches("./");
                let is_asset = [".js", ".css", ".png", ".ico", ".woff2"]
                    .iter()
                    .any(|ext| piece.ends_with(ext));
                if is_asset && !piece.contains([' ', '<', '>', '=']) {
                    referenced.push(piece.to_string());
                }
            }
        }
        assert!(referenced.len() >= 6, "{referenced:?}");
        for name in referenced {
            assert!(lookup(&name).is_some(), "{name} is referenced but missing");
        }
    }

    #[test]
    fn the_example_profile_and_jfr_wasm_are_not_shipped() {
        for name in names() {
            assert!(!name.starts_with("perf-vertx-"), "{name}");
            assert!(!name.ends_with(".wasm"), "{name}");
        }
        assert!(gunzip(lookup("release.txt").unwrap().gzip).starts_with("speedscope@1.25.0\n"));
    }

    #[test]
    fn lookup_only_accepts_bare_names() {
        assert!(lookup("index.html").is_some());
        for name in [
            "",
            "../index.html",
            "./index.html",
            "sub/index.html",
            "..\\index.html",
            "index.html.gz",
            ".",
            "..",
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
        let js = names().find(|name| name.ends_with(".js")).unwrap();
        assert_eq!(
            lookup(js).unwrap().content_type,
            "text/javascript; charset=utf-8"
        );
        let font = names().find(|name| name.ends_with(".woff2")).unwrap();
        assert_eq!(lookup(font).unwrap().content_type, "font/woff2");
    }
}
