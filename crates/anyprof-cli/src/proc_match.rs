//! Resolve `--name <NAME>` to a single running PID without shell execution.

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessEntry {
    pub pid: u32,
    pub comm: Option<String>,
    pub exe_name: Option<String>,
    pub cmdline: Vec<String>,
}

impl ProcessEntry {
    fn summary(&self) -> String {
        if !self.cmdline.is_empty() {
            let joined = self.cmdline.join(" ");
            if joined.chars().count() > 64 {
                let prefix: String = joined.chars().take(64).collect();
                return format!("{prefix}...");
            }
            return joined;
        }
        self.comm
            .clone()
            .or_else(|| self.exe_name.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Find the unique running PID matching `query` (excluding `self_pid`).
pub(crate) fn resolve_pid_by_name(query: &str) -> Result<u32> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("--name must not be empty"));
    }
    let entries = list_processes(std::process::id());
    pick_unique_match(trimmed, &entries)
}

fn pick_unique_match(query: &str, entries: &[ProcessEntry]) -> Result<u32> {
    let needle = query.to_ascii_lowercase();
    let needle_no_exe = strip_exe(&needle);

    let candidates: Vec<&ProcessEntry> = entries
        .iter()
        .filter(|entry| needle_no_exe == "anyprof" || !is_anyprof_wrapper(entry))
        .collect();

    let exe_exact: Vec<&ProcessEntry> = candidates
        .iter()
        .copied()
        .filter(|entry| is_exe_exact_match(entry, &needle, needle_no_exe))
        .collect();

    let chosen = if !exe_exact.is_empty() {
        exe_exact
    } else {
        let arg_exact: Vec<&ProcessEntry> = candidates
            .iter()
            .copied()
            .filter(|entry| is_arg_exact_match(entry, &needle, needle_no_exe))
            .collect();
        if !arg_exact.is_empty() {
            arg_exact
        } else {
            candidates
                .into_iter()
                .filter(|entry| is_substring_match(entry, &needle))
                .collect()
        }
    };

    match chosen.as_slice() {
        [] => Err(anyhow!("no running process matched --name '{query}'")),
        [single] => Ok(single.pid),
        many => {
            let preview: Vec<String> = many
                .iter()
                .take(8)
                .map(|entry| format!("pid {} ({})", entry.pid, entry.summary()))
                .collect();
            let suffix = if many.len() > 8 {
                format!(", and {} more", many.len() - 8)
            } else {
                String::new()
            };
            Err(anyhow!(
                "multiple processes ({}) matched --name '{}': {}{}; pass --pid <PID> to choose one",
                many.len(),
                query,
                preview.join(", "),
                suffix
            ))
        }
    }
}

fn is_anyprof_wrapper(entry: &ProcessEntry) -> bool {
    let is_anyprof_bin = |raw: &str| strip_exe(&basename(raw).to_ascii_lowercase()) == "anyprof";
    if entry.comm.as_deref().is_some_and(is_anyprof_bin)
        || entry.exe_name.as_deref().is_some_and(is_anyprof_bin)
        || entry
            .cmdline
            .first()
            .map(String::as_str)
            .is_some_and(is_anyprof_bin)
    {
        return true;
    }
    let Some(first) = entry.cmdline.first() else {
        return false;
    };
    let launcher = strip_exe(&basename(first).to_ascii_lowercase()).to_string();
    matches!(launcher.as_str(), "sudo" | "doas" | "pkexec" | "run0")
        && entry.cmdline.iter().skip(1).any(|arg| is_anyprof_bin(arg))
}

fn is_exe_exact_match(entry: &ProcessEntry, needle: &str, needle_no_exe: &str) -> bool {
    let matches_token = |raw: &str| {
        let base = basename(raw).to_ascii_lowercase();
        base == needle || strip_exe(&base) == needle_no_exe
    };
    entry.comm.as_deref().is_some_and(matches_token)
        || entry.exe_name.as_deref().is_some_and(matches_token)
        || entry
            .cmdline
            .first()
            .map(String::as_str)
            .is_some_and(matches_token)
}

fn is_arg_exact_match(entry: &ProcessEntry, needle: &str, needle_no_exe: &str) -> bool {
    let matches_token = |raw: &str| {
        let base = basename(raw).to_ascii_lowercase();
        base == needle || strip_exe(&base) == needle_no_exe
    };
    entry.cmdline.iter().skip(1).any(|arg| matches_token(arg))
}

fn is_substring_match(entry: &ProcessEntry, needle: &str) -> bool {
    if entry
        .comm
        .as_deref()
        .is_some_and(|c| c.to_ascii_lowercase().contains(needle))
    {
        return true;
    }
    if entry
        .exe_name
        .as_deref()
        .is_some_and(|e| e.to_ascii_lowercase().contains(needle))
    {
        return true;
    }
    entry
        .cmdline
        .iter()
        .any(|arg| arg.to_ascii_lowercase().contains(needle))
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn strip_exe(name: &str) -> &str {
    name.strip_suffix(".exe").unwrap_or(name)
}

fn list_processes(exclude_pid: u32) -> Vec<ProcessEntry> {
    #[cfg(target_os = "linux")]
    {
        list_linux(exclude_pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        list_ps(exclude_pid)
    }
    #[cfg(windows)]
    {
        list_windows(exclude_pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = exclude_pid;
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn list_linux(exclude_pid: u32) -> Vec<ProcessEntry> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in dir.flatten() {
        let Some(text) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = text.parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == exclude_pid {
            continue;
        }
        let Some(snap) = anyprof_core::inspect(pid) else {
            continue;
        };
        let exe_name = snap
            .exe
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .map(|n| n.strip_suffix(" (deleted)").unwrap_or(&n).to_string());
        out.push(ProcessEntry {
            pid,
            comm: snap.comm,
            exe_name,
            cmdline: snap.cmdline,
        });
    }
    out.sort_by_key(|entry| entry.pid);
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn list_ps(exclude_pid: u32) -> Vec<ProcessEntry> {
    use std::process::{Command, Stdio};

    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,comm="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let args_by_pid = ps_args_map();
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        let Some((pid_part, comm_part)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_part.trim().parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == exclude_pid {
            continue;
        }
        let comm = comm_part.trim();
        if comm.is_empty() {
            continue;
        }
        let cmdline = args_by_pid
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| vec![comm.to_string()]);
        out.push(ProcessEntry {
            pid,
            comm: Some(comm.to_string()),
            exe_name: Some(basename(comm).to_string()),
            cmdline,
        });
    }
    out.sort_by_key(|entry| entry.pid);
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_args_map() -> std::collections::HashMap<u32, Vec<String>> {
    use std::process::{Command, Stdio};

    let mut map = std::collections::HashMap::new();
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,args="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return map;
    };
    if !output.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        let Some((pid_part, args_part)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_part.trim().parse::<u32>() else {
            continue;
        };
        let tokens: Vec<String> = args_part.split_whitespace().map(str::to_string).collect();
        if !tokens.is_empty() {
            map.insert(pid, tokens);
        }
    }
    map
}

#[cfg(windows)]
fn list_windows(exclude_pid: u32) -> Vec<ProcessEntry> {
    use std::process::{Command, Stdio};

    let Ok(output) = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((name, pid)) = parse_tasklist_csv_line(line) else {
            continue;
        };
        if pid == 0 || pid == exclude_pid {
            continue;
        }
        out.push(ProcessEntry {
            pid,
            comm: Some(name.clone()),
            exe_name: Some(name.clone()),
            cmdline: vec![name],
        });
    }
    out.sort_by_key(|entry| entry.pid);
    out
}

#[cfg(any(windows, test))]
fn parse_tasklist_csv_line(line: &str) -> Option<(String, u32)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("INFO:") {
        return None;
    }
    let rest = line.strip_prefix('"')?;
    let name_end = rest.find('"')?;
    let name = rest[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let after_name = rest[name_end + 1..].trim_start();
    let after_comma = after_name.strip_prefix(',')?.trim_start();
    let pid_str = if let Some(quoted) = after_comma.strip_prefix('"') {
        let end = quoted.find('"')?;
        &quoted[..end]
    } else {
        after_comma.split(',').next()?.trim()
    };
    let pid = pid_str.trim().parse::<u32>().ok()?;
    Some((name.to_string(), pid))
}

#[cfg(test)]
mod tests {
    use super::{ProcessEntry, parse_tasklist_csv_line, pick_unique_match};

    fn entry(pid: u32, comm: &str, cmdline: &[&str]) -> ProcessEntry {
        ProcessEntry {
            pid,
            comm: Some(comm.to_string()),
            exe_name: Some(comm.to_string()),
            cmdline: cmdline.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn exact_basename_wins_over_substring_neighbors() {
        let entries = vec![
            entry(100, "node", &["/usr/bin/node", "server.js"]),
            entry(101, "nodemon", &["/usr/bin/nodemon", "server.js"]),
        ];
        assert_eq!(pick_unique_match("node", &entries).unwrap(), 100);
        assert_eq!(pick_unique_match("node.exe", &entries).unwrap(), 100);
    }

    #[test]
    fn substring_fallback_matches_script_argument_and_reports_ambiguity() {
        let entries = vec![
            entry(201, "python3", &["python3", "/srv/billing_worker.py"]),
            entry(202, "python3", &["python3", "/srv/search_worker.py"]),
        ];
        assert_eq!(pick_unique_match("billing_worker", &entries).unwrap(), 201);

        let err = pick_unique_match("python3", &entries)
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple processes (2)"), "{err}");
        assert!(err.contains("pid 201"), "{err}");
        assert!(err.contains("pid 202"), "{err}");
        assert!(err.contains("--pid"), "{err}");

        let miss = pick_unique_match("postgres", &entries)
            .unwrap_err()
            .to_string();
        assert!(miss.contains("no running process matched"), "{miss}");
    }

    #[test]
    fn sudo_wrapper_invoking_anyprof_does_not_collide_with_target_name() {
        let entries = vec![
            entry(7641, "aptarget7508", &["/tmp/aptarget7508"]),
            entry(
                7674,
                "sudo",
                &[
                    "sudo",
                    "-n",
                    "/usr/local/bin/anyprof",
                    "rec",
                    "-n",
                    "aptarget7508",
                ],
            ),
            entry(
                8100,
                "sudo",
                &[
                    "sudo",
                    "-n",
                    "/usr/local/bin/anyprof",
                    "rec",
                    "-n",
                    "billing_worker",
                ],
            ),
            entry(8090, "python3", &["python3", "/srv/billing_worker.py"]),
        ];
        assert_eq!(pick_unique_match("aptarget7508", &entries).unwrap(), 7641);
        assert_eq!(pick_unique_match("billing_worker", &entries).unwrap(), 8090);
    }

    #[test]
    fn tasklist_csv_extracts_image_and_pid() {
        assert_eq!(
            parse_tasklist_csv_line(r#""node.exe","4812","Console","1","42,100 K""#),
            Some(("node.exe".to_string(), 4812))
        );
        assert_eq!(parse_tasklist_csv_line("INFO: No tasks"), None);
    }
}
