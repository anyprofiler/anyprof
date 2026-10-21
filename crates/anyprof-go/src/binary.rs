use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Magic that the Go linker plants in binaries built with the standard toolchain.
const GO_BUILD_INFO_MAGIC: &[u8] = b"\xffGo buildinf:";

/// Read buffer. The overlap of `MAGIC.len() - 1` bytes catches a magic that
/// straddles two reads.
const CHUNK: usize = 64 * 1024;

/// Stop after this many bytes. Go's build info is near the start of typical
/// binaries; the cap keeps detection from reading multi-gigabyte files.
const MAX_SCAN: u64 = 256 * 1024 * 1024;

pub(crate) fn contains_go_build_info(path: &Path) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let mut pending = Vec::with_capacity(CHUNK + GO_BUILD_INFO_MAGIC.len());
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
        if contains_magic(&pending) {
            return Ok(true);
        }
        let keep = GO_BUILD_INFO_MAGIC.len() - 1;
        if pending.len() > keep {
            pending.drain(..pending.len() - keep);
        }
        scanned += n as u64;
    }
}

fn contains_magic(bytes: &[u8]) -> bool {
    bytes
        .windows(GO_BUILD_INFO_MAGIC.len())
        .any(|window| window == GO_BUILD_INFO_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::{contains_go_build_info, contains_magic, CHUNK, GO_BUILD_INFO_MAGIC};
    use std::io::Write;

    #[test]
    fn magic_must_include_the_leading_marker_byte() {
        assert!(contains_magic(b"xx\xffGo buildinf:yy"));
        assert!(!contains_magic(b"Go buildinf:"));
        assert!(!contains_magic(b""));
        assert!(!contains_magic(
            &GO_BUILD_INFO_MAGIC[..GO_BUILD_INFO_MAGIC.len() - 1]
        ));
    }

    #[test]
    fn scan_finds_magic_that_crosses_a_chunk_boundary() {
        let path = std::env::temp_dir().join(format!("anyprof-go-magic-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        let prefix = CHUNK - 2;
        file.write_all(&vec![b'a'; prefix]).unwrap();
        file.write_all(GO_BUILD_INFO_MAGIC).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        let found = contains_go_build_info(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(found);
    }

    #[test]
    fn scan_rejects_a_file_without_the_magic() {
        let path = std::env::temp_dir().join(format!("anyprof-go-plain-{}", std::process::id()));
        std::fs::write(&path, b"this is not a go binary").unwrap();
        let found = contains_go_build_info(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!found);
    }
}
