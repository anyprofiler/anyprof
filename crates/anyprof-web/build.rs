use std::io::Write;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for name in ["index.html", "app.css", "app.js"] {
        let source = manifest.join("viewer").join(name);
        println!("cargo:rerun-if-changed={}", source.display());
        let bytes = std::fs::read(&source).unwrap_or_else(|err| {
            panic!("reading {}: {err}", source.display());
        });
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&bytes).unwrap();
        let gzip = encoder.finish().unwrap();
        std::fs::write(out.join(format!("{name}.gz")), gzip).unwrap();
    }
}
