//! `/proc/<pid>/maps` lines the Linux sampler turns into modules.
//!
//! Anonymous mappings and pseudo files (`[stack]`, `[vdso]`) are kept only
//! when they have a path that is a real file. The sampler groups the lines.

#![cfg_attr(
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )),
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

/// Display module name taken from the mapped path before `/proc/<pid>/root`
/// rewriting, so `/proc/42/root/app/service` is still named `service`.
pub(crate) fn module_name(mapped_path: &str) -> String {
    let clean = live_path(mapped_path);
    std::path::Path::new(clean)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(clean)
        .to_string()
}

/// Candidate path inside `pid`'s mount namespace (`/proc/<pid>/root/...`).
///
/// Rejects relative paths, `"/"`, NUL bytes, and any `..` segment so a crafted
/// or unusual mapping line cannot escape `/proc/<pid>/root` or yield `"root"`
/// as its file name.
pub(crate) fn proc_root_candidate(pid: u32, mapped_path: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path, PathBuf};

    let clean = live_path(mapped_path);
    if pid == 0 || !clean.starts_with('/') || clean.contains('\0') {
        return None;
    }
    let mut rel = PathBuf::new();
    for component in Path::new(clean).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => rel.push(part),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    if rel.as_os_str().is_empty() {
        return None;
    }
    let mut rooted = PathBuf::from(format!("/proc/{pid}/root"));
    rooted.push(rel);
    Some(rooted)
}

/// Host-openable path for a file mapped into `pid` on Linux.
///
/// For another process (`pid != std::process::id()`), prefers
/// `/proc/<pid>/root<mapped_path>` when that regular file is readable so
/// container binaries and libraries resolve inside the target's mount
/// namespace instead of missing or opening a mismatched host library.
pub(crate) fn resolve_mapped_path(pid: u32, mapped_path: &str) -> std::path::PathBuf {
    resolve_mapped_path_with(pid, std::process::id(), mapped_path, is_readable_file)
}

fn resolve_mapped_path_with(
    pid: u32,
    self_pid: u32,
    mapped_path: &str,
    readable_file: impl Fn(&std::path::Path) -> bool,
) -> std::path::PathBuf {
    let clean = live_path(mapped_path);
    if pid != self_pid
        && let Some(candidate) = proc_root_candidate(pid, clean)
        && readable_file(&candidate)
    {
        return candidate;
    }
    std::path::PathBuf::from(clean)
}

fn is_readable_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) && std::fs::File::open(path).is_ok()
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
    use std::path::{Path, PathBuf};

    use super::{
        live_path, module_name, parse_maps, proc_root_candidate, resolve_mapped_path_with,
    };

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

    #[test]
    fn proc_root_joins_absolute_paths_and_rejects_traversal_or_bare_root() {
        assert_eq!(
            proc_root_candidate(4242, "/app/server"),
            Some(PathBuf::from("/proc/4242/root/app/server"))
        );
        assert_eq!(
            proc_root_candidate(4242, "/usr/lib/libc.so.6 (deleted)"),
            Some(PathBuf::from("/proc/4242/root/usr/lib/libc.so.6"))
        );
        assert_eq!(proc_root_candidate(0, "/app/server"), None);
        assert_eq!(proc_root_candidate(4242, "/"), None);
        assert_eq!(proc_root_candidate(4242, "relative/lib.so"), None);
        assert_eq!(proc_root_candidate(4242, "/app/../../etc/passwd"), None);
        assert_eq!(proc_root_candidate(4242, "/../bin/sh"), None);
    }

    #[test]
    fn resolve_mapped_path_prefers_readable_proc_root_for_other_pids_and_keeps_module_name() {
        let mapped = "/opt/jdk/lib/server/libjvm.so";
        assert_eq!(module_name(mapped), "libjvm.so");
        assert_eq!(module_name("/usr/lib/libc.so.6 (deleted)"), "libc.so.6");
        assert_eq!(module_name("/"), "/");

        let resolved = resolve_mapped_path_with(200, 100, mapped, |candidate| {
            candidate == Path::new("/proc/200/root/opt/jdk/lib/server/libjvm.so")
        });
        assert_eq!(
            resolved,
            PathBuf::from("/proc/200/root/opt/jdk/lib/server/libjvm.so")
        );
        assert_eq!(module_name(mapped), "libjvm.so");

        // Same pid skips `/proc/<pid>/root` indirection.
        let same = resolve_mapped_path_with(100, 100, mapped, |_| true);
        assert_eq!(same, PathBuf::from(mapped));

        // Unreadable `/proc/<pid>/root/...` falls back to the mapped path.
        let fallback = resolve_mapped_path_with(200, 100, mapped, |_| false);
        assert_eq!(fallback, PathBuf::from(mapped));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_self_root_resolves_the_current_executable_without_naming_it_root() {
        let exe = std::fs::read_link("/proc/self/exe").expect("read /proc/self/exe");
        let mapped = exe.to_string_lossy();
        let pid = std::process::id();
        let rooted = proc_root_candidate(pid, &mapped).expect("proc root path");
        assert!(
            rooted.starts_with(format!("/proc/{pid}/root/")),
            "{}",
            rooted.display()
        );
        assert!(super::is_readable_file(&rooted), "{}", rooted.display());
        let resolved = resolve_mapped_path_with(pid, 0, &mapped, super::is_readable_file);
        assert_eq!(resolved, rooted);
        assert_ne!(module_name(&mapped), "root");
        assert_eq!(
            Some(module_name(&mapped).as_str()),
            exe.file_name().and_then(|name| name.to_str())
        );
    }
}
