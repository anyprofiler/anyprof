#[cfg(target_os = "macos")]
use std::io::{IsTerminal, Write};
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use anyhow::{Context, anyhow, bail};

use crate::cli::SetupArgs;

#[cfg(target_os = "macos")]
const ENTITLEMENTS_PLIST: &str = include_str!("../../../assets/macos/entitlements.plist");

pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        run_macos(args)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = args;
        let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let ptrace = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "absent".to_string());
        eprintln!(
            "Linux does not use macOS debugger code-signing entitlements.\n\
             Current kernel settings: perf_event_paranoid={paranoid} (<= 2 allows unprivileged user-space sampling), \
             yama.ptrace_scope={ptrace} (0 allows non-child Python/Java VM reads; 1 allows 'anyprof rec -- <cmd>')."
        );
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = args;
        eprintln!(
            "Windows does not use macOS debugger code-signing entitlements.\n\
             Same-user x64 processes can be sampled directly without Administrator privileges."
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = args;
        eprintln!("Debugger entitlement code-signing setup applies only to macOS.");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn run_macos(args: SetupArgs) -> anyhow::Result<()> {
    let exe: PathBuf = match args.binary {
        Some(path) => path,
        None => std::env::current_exe().context("locating current anyprof executable")?,
    };
    if !exe.is_file() {
        bail!("target binary does not exist: {}", exe.display());
    }
    let identity = args.identity.as_deref().unwrap_or("-");

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing interactive prompt when stdin is not a terminal; re-run with 'anyprof setup --yes'"
            );
        }
        eprint!(
            "macOS requires the 'com.apple.security.cs.debugger' entitlement on anyprof\n\
             to attach via task_for_pid without sudo (covering Python, Java, Go, Rust, and C/C++).\n\n\
             Command to execute:\n  \
             codesign --force --options runtime --sign {identity:?} --entitlements entitlements.plist {}\n\n\
             Press Enter to continue, or Ctrl+C to cancel: ",
            exe.display()
        );
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading confirmation")?;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let plist_path = std::env::temp_dir().join(format!(
        "anyprof-entitlements-{}-{nanos}.plist",
        std::process::id()
    ));
    std::fs::write(&plist_path, ENTITLEMENTS_PLIST)
        .with_context(|| format!("writing {}", plist_path.display()))?;

    let parent = exe
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let file_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "anyprof".to_string());
    // Stage onto a fresh inode before invoking `codesign --force` and atomically
    // `rename` into place so Apple Silicon's XNU vnode code-signature cache never
    // invalidates pages of the currently running executable or a cached inode.
    let staged = parent.join(format!(
        ".{file_name}.codesign-{}-{nanos}",
        std::process::id()
    ));
    std::fs::copy(&exe, &staged).with_context(|| {
        format!(
            "staging copy at {} (check directory write permissions)",
            staged.display()
        )
    })?;

    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--options", "runtime", "--sign", identity]);
    if identity != "-" && args.timestamp {
        cmd.arg("--timestamp");
    }
    cmd.arg("--entitlements").arg(&plist_path).arg(&staged);

    let sign_out = cmd.output();
    let _ = std::fs::remove_file(&plist_path);
    let sign_out = match sign_out {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&staged);
            return Err(err).context("invoking codesign");
        }
    };
    if !sign_out.status.success() {
        let _ = std::fs::remove_file(&staged);
        let stderr = String::from_utf8_lossy(&sign_out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&sign_out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(anyhow!("codesign failed ({}): {detail}", sign_out.status));
    }

    let verify = Command::new("codesign")
        .args(["--verify", "--verbose=1"])
        .arg(&staged)
        .output();
    let verify = match verify {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&staged);
            return Err(err).context("verifying signature with codesign");
        }
    };
    if !verify.status.success() {
        let _ = std::fs::remove_file(&staged);
        let stderr = String::from_utf8_lossy(&verify.stderr).trim().to_string();
        bail!("codesign verification failed: {stderr}");
    }

    if let Err(err) = std::fs::rename(&staged, &exe) {
        let _ = std::fs::remove_file(&staged);
        return Err(err).with_context(|| format!("replacing {}", exe.display()));
    }

    let dev_mode_enabled = Command::new("/usr/sbin/DevToolsSecurity")
        .arg("-status")
        .output()
        .ok()
        .is_some_and(|out| {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .to_ascii_lowercase();
            text.contains("enabled")
        });

    if dev_mode_enabled {
        eprintln!(
            "Signed {} with com.apple.security.cs.debugger (Developer Mode is enabled; 'anyprof rec' can now sample without sudo).",
            exe.display()
        );
    } else {
        eprintln!(
            "Signed {} with com.apple.security.cs.debugger.\n\
             Note: macOS Developer Mode is currently disabled. Run 'sudo DevToolsSecurity -enable' once \
             (or open Xcode) so macOS taskgated allows passwordless debugging in your user session.",
            exe.display()
        );
    }
    Ok(())
}
