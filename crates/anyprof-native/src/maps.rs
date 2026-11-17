//! `/proc/<pid>/maps` lines the Linux sampler turns into modules.
//!
//! Anonymous mappings and pseudo files (`[stack]`, `[vdso]`) are kept only
//! when they have a path that is a real file. The sampler groups the lines.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]

/// One mapping with a file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MapSpan {
    /// First byte of the mapping.
    pub start: u64,
    /// First byte past the mapping.
    pub end: u64,
    /// Offset in the file, from the maps line.
    pub offset: u64,
    /// The permission field contains `x`.
    pub executable: bool,
    /// Path as printed, including a trailing ` (deleted)` when present.
    pub path: String,
}

/// Parse `maps` text. Lines without a path are skipped.
pub(crate) fn parse_maps(text: &str) -> Vec<MapSpan> {
    let mut spans = Vec::new();
    for line in text.lines() {
        let Some(span) = parse_line(line) else {
            continue;
        };
        spans.push(span);
    }
    spans
}

/// Path with the kernel's ` (deleted)` suffix removed.
pub(crate) fn live_path(path: &str) -> &str {
    path.strip_suffix(" (deleted)").unwrap_or(path)
}

fn parse_line(line: &str) -> Option<MapSpan> {
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let offset = parts.next()?;
    let _dev = parts.next()?;
    let _inode = parts.next()?;
    let path = parts.next()?;
    if path.starts_with('[') {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    if end <= start {
        return None;
    }
    let offset = u64::from_str_radix(offset, 16).ok()?;
    Some(MapSpan {
        start,
        end,
        offset,
        executable: perms.contains('x'),
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{live_path, parse_maps};

    #[test]
    fn file_mappings_are_kept_and_pseudo_files_are_not() {
        let text = "\
555555554000-555555558000 r-xp 00001000 08:01 123 /tmp/busy
555555558000-555555559000 r--p 00005000 08:01 123 /tmp/busy
7ffff7a00000-7ffff7c00000 r-xp 00000000 08:01 456 /usr/lib/libc.so.6 (deleted)
7ffffffde000-7ffffffff000 rw-p 00000000 00:00 0 [stack]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]
";
        let spans = parse_maps(text);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].start, 0x5555_5555_4000);
        assert_eq!(spans[0].end, 0x5555_5555_8000);
        assert_eq!(spans[0].offset, 0x1000);
        assert!(spans[0].executable);
        assert!(!spans[1].executable);
        assert_eq!(spans[1].path, "/tmp/busy");
        assert_eq!(live_path(&spans[2].path), "/usr/lib/libc.so.6");
    }
}
