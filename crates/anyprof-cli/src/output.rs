//! Where a recording is written, and which format that path implies.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, anyhow};

use crate::cli::{ProfileFormat, infer_format};

/// Inputs for [`resolve_output`]. The stamp is injected so tests do not need a clock.
pub(crate) struct OutputRequest<'a> {
    /// `-o` value. `None` writes an auto-named file in `cwd`.
    pub output: Option<&'a Path>,
    /// `--format`, when the user passed it.
    pub explicit: Option<ProfileFormat>,
    /// [`anyprof_core::Backend::default_format`] for the selected backend.
    pub default_format: ProfileFormat,
    /// Backend name used in the auto-generated file name.
    pub backend: &'a str,
    /// Pid, or a sanitized browser tab. Already filesystem-safe.
    pub label: &'a str,
    /// Local timestamp `YYYYMMDD-HHMMSS`, without colons.
    pub stamp: &'a str,
    /// Directory relative paths resolve against.
    pub cwd: &'a Path,
    /// Home directory for a leading `~`.
    pub home: Option<&'a Path>,
}

/// Absolute path to write, and the format of those bytes.
pub(crate) fn resolve_output(
    request: &OutputRequest<'_>,
) -> anyhow::Result<(PathBuf, ProfileFormat)> {
    let mode = match request.output {
        None => OutputMode::Directory(normalize(request.cwd)),
        Some(raw) => classify(raw, request.cwd, request.home)?,
    };
    let format = match (request.explicit, &mode) {
        (Some(format), _) => format,
        (None, OutputMode::File(path)) => infer_format(path).unwrap_or(request.default_format),
        (None, OutputMode::Directory(_)) => request.default_format,
    };
    let path = match mode {
        OutputMode::File(path) => path,
        OutputMode::Directory(dir) => unique_name(
            &dir,
            &auto_file_name(request.backend, request.label, request.stamp, format),
        )?,
    };
    Ok((path, format))
}

/// `anyprof-node-1234-20261022-122730.cpuprofile`, with characters outside a
/// safe set replaced so the name cannot escape its directory.
pub(crate) fn auto_file_name(
    backend: &str,
    label: &str,
    stamp: &str,
    format: ProfileFormat,
) -> String {
    format!(
        "anyprof-{}-{}-{}{}",
        sanitize_component(backend),
        sanitize_component(label),
        sanitize_component(stamp),
        format.extension()
    )
}

/// Pid for a process backend, or a filesystem-safe tab selector.
pub(crate) fn profile_label(browser: bool, pid: u32, tab: Option<&str>) -> String {
    if browser {
        tab.map(sanitize_component)
            .unwrap_or_else(|| "tab".to_string())
    } else {
        pid.to_string()
    }
}

/// Local time as `YYYYMMDD-HHMMSS`.
pub(crate) fn local_stamp() -> String {
    jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string()
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = std::env::var_os("HOMEDRIVE")?;
        let path = std::env::var_os("HOMEPATH")?;
        let mut home = PathBuf::from(drive);
        home.push(path);
        Some(home)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

enum OutputMode {
    File(PathBuf),
    Directory(PathBuf),
}

fn classify(raw: &Path, cwd: &Path, home: Option<&Path>) -> anyhow::Result<OutputMode> {
    let expanded = expand_tilde(raw, home)?;
    let directory = is_dot_dir(raw)
        || is_dot_dir(&expanded)
        || ends_with_separator(raw)
        || ends_with_separator(&expanded);
    let absolute = absolute_against(cwd, &expanded);
    if directory || absolute.is_dir() {
        Ok(OutputMode::Directory(absolute))
    } else {
        Ok(OutputMode::File(absolute))
    }
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home
            .map(Path::to_path_buf)
            .context("cannot expand ~: home directory is not set");
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        let home = home.context("cannot expand ~: home directory is not set")?;
        return Ok(home.join(rest));
    }
    Ok(path.to_path_buf())
}

fn is_dot_dir(path: &Path) -> bool {
    path == Path::new(".") || path == Path::new("..")
}

fn ends_with_separator(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.ends_with('/') || text.ends_with('\\')
}

fn absolute_against(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize(&joined)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn unique_name(dir: &Path, file_name: &str) -> anyhow::Result<PathBuf> {
    let first = dir.join(file_name);
    if !first.exists() {
        return Ok(first);
    }
    let (stem, extension) = split_extension(file_name);
    for suffix in 2..10_000 {
        let candidate = dir.join(format!("{stem}-{suffix}{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "could not find a free name for {} in {}",
        file_name,
        dir.display()
    ))
}

/// Split `name.speedscope.json` into (`name`, `.speedscope.json`).
fn split_extension(file_name: &str) -> (&str, &str) {
    const COMPOUND: &[&str] = &[".speedscope.json", ".pb.gz", ".pprof.gz"];
    for extension in COMPOUND {
        if let Some(stem) = file_name.strip_suffix(extension) {
            return (stem, extension);
        }
    }
    match file_name.rfind('.') {
        Some(index) if index > 0 => (&file_name[..index], &file_name[index..]),
        _ => (file_name, ""),
    }
}

/// Keep ascii letters, digits, `.`, `-`, and `_`. Everything else becomes `-`.
pub(crate) fn sanitize_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "tab".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputRequest, local_stamp, resolve_output, sanitize_component, split_extension};
    use crate::cli::ProfileFormat;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("anyprof-output-{nanos}-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request<'a>(
        output: Option<&'a Path>,
        explicit: Option<ProfileFormat>,
        default_format: ProfileFormat,
        backend: &'a str,
        label: &'a str,
        cwd: &'a Path,
        home: Option<&'a Path>,
    ) -> OutputRequest<'a> {
        OutputRequest {
            output,
            explicit,
            default_format,
            backend,
            label,
            stamp: "20261022-122730",
            cwd,
            home,
        }
    }

    #[test]
    fn omitted_output_uses_the_backend_default_in_the_cwd() {
        let cwd = scratch();
        let (path, format) = resolve_output(&request(
            None,
            None,
            ProfileFormat::Pprof,
            "native",
            "42",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Pprof);
        assert_eq!(path, cwd.join("anyprof-native-42-20261022-122730.pb.gz"));
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn directory_dot_and_trailing_separator_are_auto_named() {
        let cwd = scratch();
        let nested = cwd.join("profiles.cpuprofile");
        fs::create_dir_all(&nested).unwrap();

        let (in_dir, format) = resolve_output(&request(
            Some(&nested),
            None,
            ProfileFormat::Cpuprofile,
            "node",
            "7",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Cpuprofile);
        assert_eq!(
            in_dir,
            nested.join("anyprof-node-7-20261022-122730.cpuprofile")
        );

        let dot = PathBuf::from(".");
        let (here, _) = resolve_output(&request(
            Some(&dot),
            None,
            ProfileFormat::Pprof,
            "go",
            "9",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(here, cwd.join("anyprof-go-9-20261022-122730.pb.gz"));

        let trailing = PathBuf::from(format!("fresh{}", std::path::MAIN_SEPARATOR));
        let (created, format) = resolve_output(&request(
            Some(&trailing),
            None,
            ProfileFormat::Speedscope,
            "node",
            "3",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Speedscope);
        assert_eq!(
            created,
            cwd.join("fresh")
                .join("anyprof-node-3-20261022-122730.speedscope.json")
        );
        assert!(!created.exists());
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn explicit_file_keeps_its_path_and_extension_can_override_the_default() {
        let cwd = scratch();
        let relative = Path::new("sub/cpu.collapsed");
        let (path, format) = resolve_output(&request(
            Some(relative),
            None,
            ProfileFormat::Cpuprofile,
            "node",
            "1",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Collapsed);
        assert_eq!(path, cwd.join("sub").join("cpu.collapsed"));

        let bare = Path::new("cpu");
        let (bare_path, bare_format) = resolve_output(&request(
            Some(bare),
            None,
            ProfileFormat::Pprof,
            "native",
            "1",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(bare_format, ProfileFormat::Pprof);
        assert_eq!(bare_path, cwd.join("cpu"));

        let named = cwd.join("cpu.cpuprofile");
        let (kept, format) = resolve_output(&request(
            Some(&named),
            Some(ProfileFormat::Pprof),
            ProfileFormat::Cpuprofile,
            "node",
            "1",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Pprof);
        assert_eq!(kept, named);
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn absolute_paths_ignore_cwd_and_tilde_expands() {
        let cwd = scratch();
        let elsewhere = scratch();
        let absolute = elsewhere.join("abs.cpuprofile");
        let (path, format) = resolve_output(&request(
            Some(&absolute),
            None,
            ProfileFormat::Collapsed,
            "node",
            "1",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Cpuprofile);
        assert_eq!(path, absolute);

        let home = scratch();
        let tilde = Path::new("~/prof.cpuprofile");
        let (home_path, _) = resolve_output(&request(
            Some(tilde),
            None,
            ProfileFormat::Pprof,
            "node",
            "1",
            &cwd,
            Some(&home),
        ))
        .unwrap();
        assert_eq!(home_path, home.join("prof.cpuprofile"));

        let just_home = Path::new("~");
        let (auto, format) = resolve_output(&request(
            Some(just_home),
            None,
            ProfileFormat::Cpuprofile,
            "browser",
            "example.com",
            &cwd,
            Some(&home),
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Cpuprofile);
        assert_eq!(
            auto,
            home.join("anyprof-browser-example.com-20261022-122730.cpuprofile")
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&elsewhere);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn collisions_suffix_before_the_full_extension() {
        let cwd = scratch();
        let first = cwd.join("anyprof-node-4-20261022-122730.pb.gz");
        let second = cwd.join("anyprof-node-4-20261022-122730-2.pb.gz");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        let (path, format) = resolve_output(&request(
            None,
            None,
            ProfileFormat::Pprof,
            "node",
            "4",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(format, ProfileFormat::Pprof);
        assert_eq!(path, cwd.join("anyprof-node-4-20261022-122730-3.pb.gz"));

        let explicit = cwd.join("cpu.cpuprofile");
        fs::write(&explicit, b"keep").unwrap();
        let (same, _) = resolve_output(&request(
            Some(Path::new("cpu.cpuprofile")),
            None,
            ProfileFormat::Collapsed,
            "node",
            "4",
            &cwd,
            None,
        ))
        .unwrap();
        assert_eq!(same, explicit);
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn missing_home_rejects_a_tilde() {
        let cwd = scratch();
        let err = resolve_output(&request(
            Some(Path::new("~/cpu.cpuprofile")),
            None,
            ProfileFormat::Cpuprofile,
            "node",
            "1",
            &cwd,
            None,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("home"), "{err}");
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn stamp_has_no_colons_and_labels_are_filesystem_safe() {
        let stamp = local_stamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert!(!stamp.contains(':'), "{stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-');
        assert_eq!(
            sanitize_component("https://example.com/a"),
            "https---example.com-a"
        );
        assert_eq!(sanitize_component("///"), "tab");
        assert_eq!(
            split_extension("anyprof-node-1-20261022-122730.speedscope.json"),
            ("anyprof-node-1-20261022-122730", ".speedscope.json")
        );
        assert_eq!(
            split_extension("anyprof-go-1-20261022-122730.pb.gz"),
            ("anyprof-go-1-20261022-122730", ".pb.gz")
        );
    }
}
