//! Recognize a Rust executable without attaching to it.
//!
//! Executables do not carry the `.rustc` metadata section. That section is
//! for dylibs and proc-macros. What rustc does put in a normal binary, and
//! what `strip` leaves behind on ELF, is one of:
//!
//! - the `.comment` string `rustc version <version>` (ELF, including
//!   `#![no_std]` binaries);
//! - standard-library text such as `called \`Option::unwrap()\`` and
//!   `fatal runtime error:`;
//! - a remapped panic path `/rustc/<40 hex chars>/library/`.
//!
//! The scan is bounded and stops at the first hit. It does not ptrace, and
//! it does not read past [`MAX_SCAN`]. A Mach-O or PE `no_std` binary that
//! was stripped and contains none of these strings is not detected; passing
//! `--backend rust` still samples it.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Stop after this many bytes. The markers live in `.rodata` or `.comment`,
/// which the linker places before DWARF, so a debug binary does not have to
/// be read to the end.
const MAX_SCAN: u64 = 32 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Longest marker, so a hit that straddles two reads is not missed.
const MARKERS: &[&[u8]] = &[
    b"rustc version ",
    b"called `Option::unwrap()` on a `None` value",
    b"called `Result::unwrap()` on an `Err` value",
    b"fatal runtime error: ",
];

const RUSTC_PATH: &[u8] = b"/rustc/";
const LIBRARY_PATH: &[u8] = b"/library/";
/// `/rustc/` + 40 hex digits + `/library/`.
const RUSTC_LIBRARY_LEN: usize = 7 + 40 + 9;

/// Whether `path` looks like a binary rustc produced.
///
/// I/O errors are "not recognized" rather than a sampling failure. Detection
/// must not attach, and a file that cannot be read falls through to the
/// native backend.
#[must_use]
pub fn file_is_rust(path: &Path) -> bool {
    match scan(path) {
        Ok(found) => found,
        Err(err) => {
            tracing::debug!(path = %path.display(), %err, "rust image scan failed");
            false
        }
    }
}

fn scan(path: &Path) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let keep = MARKERS
        .iter()
        .map(|marker| marker.len())
        .max()
        .unwrap_or(1)
        .max(RUSTC_LIBRARY_LEN)
        - 1;
    let mut pending = Vec::with_capacity(CHUNK + keep);
    let mut buf = vec![0_u8; CHUNK];
    let mut scanned = 0_u64;
    loop {
        if scanned >= MAX_SCAN {
            return Ok(false);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        pending.extend_from_slice(&buf[..n]);
        if contains_marker(&pending) {
            return Ok(true);
        }
        if pending.len() > keep {
            pending.drain(..pending.len() - keep);
        }
        scanned += n as u64;
    }
}

fn contains_marker(bytes: &[u8]) -> bool {
    if MARKERS.iter().any(|marker| find_slice(bytes, marker)) {
        return true;
    }
    rustc_library_path(bytes)
}

fn rustc_library_path(bytes: &[u8]) -> bool {
    let mut start = 0;
    while let Some(at) = find_from(bytes, RUSTC_PATH, start) {
        let hex_at = at + RUSTC_PATH.len();
        let lib_at = hex_at + 40;
        let hex = bytes.get(hex_at..lib_at);
        let library = bytes.get(lib_at..lib_at + LIBRARY_PATH.len());
        if hex.is_some_and(|digits| digits.iter().all(|byte| byte.is_ascii_hexdigit()))
            && library == Some(LIBRARY_PATH)
        {
            return true;
        }
        start = at + 1;
    }
    false
}

fn find_slice(haystack: &[u8], needle: &[u8]) -> bool {
    find_from(haystack, needle, 0).is_some()
}

fn find_from(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start >= haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

#[cfg(test)]
mod tests {
    use super::{CHUNK, contains_marker, file_is_rust, scan};
    use std::io::Write;

    #[test]
    fn markers_match_rustc_output_and_not_a_c_comment() {
        assert!(contains_marker(b"xxrustc version 1.99.1yy"));
        assert!(contains_marker(
            b"called `Option::unwrap()` on a `None` value"
        ));
        assert!(contains_marker(b"prefix fatal runtime error: aborted"));
        let mut path = b"/rustc/".to_vec();
        path.extend(std::iter::repeat_n(b'a', 40));
        path.extend(b"/library/std/src/panicking.rs");
        assert!(contains_marker(&path));
        assert!(!contains_marker(b"GCC: (Ubuntu 14.2.0) 14.2.0"));
        assert!(!contains_marker(b"Go buildinf:"));
        assert!(!contains_marker(b"/rustc/not-a-commit/library/"));
        assert!(!contains_marker(b""));
    }

    #[test]
    fn scan_finds_a_marker_that_crosses_a_chunk_boundary() {
        let path = std::env::temp_dir().join(format!(
            "anyprof-rust-marker-{}-{}",
            std::process::id(),
            marker_nonce()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        let marker = b"called `Option::unwrap()` on a `None` value";
        let prefix = CHUNK - 4;
        file.write_all(&vec![b'a'; prefix]).unwrap();
        file.write_all(marker).unwrap();
        drop(file);
        let found = scan(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(found);
    }

    #[test]
    fn scan_stops_without_reading_a_plain_file_as_rust() {
        let path = std::env::temp_dir().join(format!(
            "anyprof-rust-plain-{}-{}",
            std::process::id(),
            marker_nonce()
        ));
        std::fs::write(&path, b"this is a C program\0GCC: (GNU) 14.2.0").unwrap();
        let found = file_is_rust(&path);
        let _ = std::fs::remove_file(&path);
        assert!(!found);
        assert!(!file_is_rust(std::path::Path::new(
            "/no/such/anyprof-rust-binary"
        )));
    }

    fn marker_nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }
}
