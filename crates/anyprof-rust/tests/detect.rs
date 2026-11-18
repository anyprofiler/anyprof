//! The bytes rustc actually emits are what detection has to recognize.
//!
//! Unit tests cover synthetic markers. This compiles a real binary so a
//! rustc change that drops `rustc version` and the standard-library strings
//! fails here instead of silently labeling Rust processes as native.

use std::process::Command;

use anyprof_native::file_is_rust;

#[test]
fn a_rustc_binary_is_detected_and_strip_does_not_hide_it() {
    let dir = std::env::temp_dir().join(format!(
        "anyprof-rust-detect-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("fixture.rs");
    let binary = dir.join(exe_name("fixture"));
    std::fs::write(
        &source,
        "fn main() {\n    let _ = option_none();\n}\nfn option_none() -> Option<u8> { None }\n",
    )
    .unwrap();
    let rustc = rustc_path();
    let output = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-name",
            "anyprof_rust_detect",
            "-C",
            "opt-level=0",
            "-o",
        ])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn {rustc}: {err}"));
    assert!(
        output.status.success(),
        "rustc failed\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        file_is_rust(&binary),
        "rustc output was not recognized as Rust"
    );

    #[cfg(unix)]
    {
        let stripped = dir.join(exe_name("fixture-stripped"));
        std::fs::copy(&binary, &stripped).unwrap();
        let strip = Command::new("strip")
            .arg("--strip-all")
            .arg(&stripped)
            .output();
        if let Ok(output) = strip
            && output.status.success()
        {
            assert!(
                file_is_rust(&stripped),
                "strip removed every Rust marker; detection must keep working for a stripped std binary"
            );
        }
    }

    let plain = dir.join("plain.bin");
    std::fs::write(&plain, b"GCC: (GNU) 14.2.0\0int main(){return 0;}").unwrap();
    assert!(!file_is_rust(&plain));
    let _ = std::fs::remove_dir_all(&dir);
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn rustc_path() -> String {
    std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string())
}
