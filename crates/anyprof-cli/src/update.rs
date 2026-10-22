//! `anyprof update`, backed by cargo-dist's [`axoupdater`] crate.
//!
//! The shell and PowerShell installers write a receipt for the `anyprof-cli`
//! app (the same name as `anyprof-cli-installer.sh`). That receipt is what
//! tells the updater which GitHub release to fetch and where the binary lives.
//! `install-updater` stays off in `dist-workspace.toml`; this subcommand is the
//! in-process updater cargo-dist documents for axoupdater-as-a-library.
//!
//! The installer that axoupdater runs is not checked against a separate
//! checksum: cargo-dist 0.33 publishes `sha256.sum` for the archives only. The
//! installer embeds the archive's SHA-256 and verifies it before unpacking.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use axoupdater::{AxoUpdater, AxoupdateError, ReleaseSource, ReleaseSourceType, Version};

/// cargo-dist app name. The published installers are `anyprof-cli-installer.sh`
/// and `anyprof-cli-installer.ps1`.
const APP_NAME: &str = "anyprof-cli";
const OWNER: &str = "anyprofiler";
const REPO: &str = "anyprof";

/// Variables that move the release lookup (read by axoupdater) or the archive
/// download (read by the installer it spawns) off github.com.
const HOST_OVERRIDES: [&str; 4] = [
    "ANYPROF_CLI_INSTALLER_GITHUB_BASE_URL",
    "ANYPROF_CLI_INSTALLER_GHE_BASE_URL",
    "ANYPROF_CLI_DOWNLOAD_URL",
    "INSTALLER_DOWNLOAD_URL",
];

/// The installer reads this before the `CARGO_DIST_FORCE_INSTALL_DIR` that
/// axoupdater sets from the receipt, so a stale value would install the new
/// binary somewhere else (and on Windows still delete the renamed original).
const INSTALL_DIR_OVERRIDE: &str = "ANYPROF_CLI_INSTALL_DIR";

const SHELL_INSTALLER: &str = "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anyprofiler/anyprof/releases/latest/download/anyprof-cli-installer.sh | sh";
const POWERSHELL_INSTALLER: &str = "powershell -ExecutionPolicy Bypass -c \"irm https://github.com/anyprofiler/anyprof/releases/latest/download/anyprof-cli-installer.ps1 | iex\"";

pub fn run() -> anyhow::Result<()> {
    clear_installer_overrides();

    let mut updater = AxoUpdater::new_for(APP_NAME);
    updater.load_receipt().map_err(receipt_failure)?;
    if let Some(source) = updater.source.as_ref().filter(|s| !is_pinned_source(s)) {
        return Err(anyhow!(
            "the install receipt points at {} {}/{} (app {}), not github.com/{OWNER}/{REPO}. \
             anyprof update only installs releases from that repository.\n{}",
            source.release_type,
            source.owner,
            source.name,
            source.app_name,
            reinstall_hint()
        ));
    }
    if !updater
        .check_receipt_is_for_this_executable()
        .map_err(location_failure)?
    {
        return Err(anyhow!(
            "this anyprof binary is not the copy installed by the GitHub release installer.\n{}",
            reinstall_hint()
        ));
    }

    // The receipt records the version the installer wrote, which can lag the
    // running binary. Compare and report against the binary itself.
    let current = env!("CARGO_PKG_VERSION");
    let current_version = current
        .parse::<Version>()
        .with_context(|| format!("anyprof's own version {current} is not semver"))?;
    updater
        .set_current_version(current_version.clone())
        .map_err(lookup_failure)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the update runtime")?;
    let latest = runtime
        .block_on(updater.query_new_version())
        .map_err(lookup_failure)?
        .cloned()
        .ok_or_else(|| anyhow!("github.com/{OWNER}/{REPO} has no releases to update to"))?;
    match latest.cmp(&current_version) {
        Ordering::Less => {
            println!("{}", format_newer_than_latest(current, &latest.to_string()));
            return Ok(());
        }
        Ordering::Equal => {
            println!("{}", format_already_latest(current));
            return Ok(());
        }
        Ordering::Greater => {}
    }

    // On Windows run() renames the running image, and current_exe() can follow
    // that rename, so take the path first.
    let previous_exe = previous_exe_path();
    hold_interrupts_until_exit();
    match runtime.block_on(updater.run()) {
        Ok(Some(update)) => {
            println!(
                "{}",
                format_updated(current, &update.new_version.to_string())
            );
            Ok(())
        }
        Ok(None) => {
            println!("{}", format_already_latest(current));
            Ok(())
        }
        // Only Windows reports this, after the new anyprof.exe is in place.
        Err(AxoupdateError::CleanupFailed {}) => {
            println!("{}", format_updated(current, &latest.to_string()));
            eprintln!("{}", format_cleanup_warning(&previous_exe));
            Ok(())
        }
        Err(err) => Err(install_failure(err)),
    }
}

fn is_pinned_source(source: &ReleaseSource) -> bool {
    source.release_type == ReleaseSourceType::GitHub
        && source.owner == OWNER
        && source.name == REPO
        && source.app_name == APP_NAME
}

fn clear_installer_overrides() {
    let host_note = format!("updates come from github.com/{OWNER}/{REPO}");
    let install_dir_note = "updates install where the release installer put this copy".to_string();
    let overrides = HOST_OVERRIDES
        .iter()
        .map(|name| (*name, &host_note))
        .chain([(INSTALL_DIR_OVERRIDE, &install_dir_note)]);
    for (name, note) in overrides {
        if std::env::var_os(name).is_some() {
            eprintln!("anyprof: ignoring {name}; {note}");
            // SAFETY: this runs before the updater builds its runtime or spawns
            // the installer, so no other thread can be reading the environment.
            unsafe { std::env::remove_var(name) };
        }
    }
}

/// On Windows axoupdater renames the running `anyprof.exe` aside while the
/// installer copies the new one in, and renames it back only if this process
/// is still alive when the installer fails. Ctrl+C also reaches the installer,
/// so ignoring it here is enough for that rollback to run.
fn hold_interrupts_until_exit() {
    if let Err(err) = ctrlc::set_handler(|| {
        eprintln!("anyprof: update in progress; waiting for the installer to exit");
    }) {
        tracing::warn!("could not hold Ctrl+C during the update: {err}");
    }
}

/// Where axoupdater leaves the old executable on Windows.
fn previous_exe_path() -> PathBuf {
    let mut path: OsString = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("anyprof.exe"))
        .into_os_string();
    path.push(".previous.exe");
    PathBuf::from(path)
}

fn format_updated(old: &str, new: &str) -> String {
    format!("anyprof {old} → {new}")
}

fn format_already_latest(version: &str) -> String {
    format!("anyprof {version} is already the latest release")
}

fn format_newer_than_latest(current: &str, latest: &str) -> String {
    format!("anyprof {current} is newer than the latest release ({latest}); nothing to update")
}

fn format_cleanup_warning(previous: &std::path::Path) -> String {
    format!(
        "anyprof: the new version is installed, but the old copy at {} could not be deleted. Delete it by hand.",
        previous.display()
    )
}

fn reinstall_hint() -> String {
    format!(
        "Install or reinstall with the GitHub release installer, then run `anyprof update` again.\n  \
         macOS and Linux:\n    {SHELL_INSTALLER}\n  \
         Windows (PowerShell):\n    {POWERSHELL_INSTALLER}"
    )
}

fn writable_hint() -> String {
    match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.display().to_string()))
    {
        Some(dir) => format!("Check that you can write to {dir}."),
        None => "Check that you can write to the install directory.".to_string(),
    }
}

fn receipt_failure(err: AxoupdateError) -> anyhow::Error {
    match &err {
        AxoupdateError::NoReceipt { .. } | AxoupdateError::NoHome {} => anyhow!(
            "anyprof was not installed with the GitHub release installer, so it cannot update itself ({err}).\n{}",
            reinstall_hint()
        ),
        _ => anyhow!(
            "anyprof could not read its install receipt ({err}).\n{}",
            reinstall_hint()
        ),
    }
}

fn location_failure(err: AxoupdateError) -> anyhow::Error {
    anyhow!(
        "anyprof could not work out where this binary is installed ({err}).\n{}",
        reinstall_hint()
    )
}

fn lookup_failure(err: AxoupdateError) -> anyhow::Error {
    anyhow!("anyprof could not look up the latest release on github.com/{OWNER}/{REPO} ({err})")
}

fn install_failure(err: AxoupdateError) -> anyhow::Error {
    match &err {
        // The installer's output is already on the terminal, so only the exit
        // status is worth repeating.
        AxoupdateError::InstallFailed { status, .. } => anyhow!(
            "anyprof could not replace the installed binary ({}). {}\n{}",
            match status {
                Some(code) => format!("the release installer exited with status {code}"),
                None => "the release installer did not run to completion".to_string(),
            },
            writable_hint(),
            reinstall_hint()
        ),
        AxoupdateError::Io(_) => anyhow!(
            "anyprof update could not finish ({err}). {}\n{}",
            writable_hint(),
            reinstall_hint()
        ),
        other => anyhow!("anyprof update failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        APP_NAME, OWNER, REPO, format_already_latest, format_cleanup_warning,
        format_newer_than_latest, format_updated, install_failure, is_pinned_source,
        location_failure, receipt_failure, reinstall_hint,
    };
    use axoupdater::{AxoupdateError, ReleaseSource, ReleaseSourceType};

    fn source(
        release_type: ReleaseSourceType,
        owner: &str,
        name: &str,
        app: &str,
    ) -> ReleaseSource {
        ReleaseSource {
            release_type,
            owner: owner.to_string(),
            name: name.to_string(),
            app_name: app.to_string(),
        }
    }

    #[test]
    fn update_lines_name_both_versions() {
        assert_eq!(format_updated("0.1.0", "0.1.1"), "anyprof 0.1.0 → 0.1.1");
        assert_eq!(
            format_already_latest("0.1.1"),
            "anyprof 0.1.1 is already the latest release"
        );
        assert_eq!(
            format_newer_than_latest("0.1.2", "0.1.1"),
            "anyprof 0.1.2 is newer than the latest release (0.1.1); nothing to update"
        );
    }

    #[test]
    fn only_the_anyprof_github_repository_is_trusted() {
        assert!(is_pinned_source(&source(
            ReleaseSourceType::GitHub,
            OWNER,
            REPO,
            APP_NAME
        )));
        for other in [
            source(ReleaseSourceType::GitHub, "someone-else", REPO, APP_NAME),
            source(ReleaseSourceType::GitHub, OWNER, "fork", APP_NAME),
            source(ReleaseSourceType::GitHub, OWNER, REPO, "other-app"),
            source(ReleaseSourceType::Axo, OWNER, REPO, APP_NAME),
        ] {
            assert!(!is_pinned_source(&other), "{other:?}");
        }
    }

    #[test]
    fn missing_receipt_points_at_both_labelled_installers() {
        let message = receipt_failure(AxoupdateError::NoReceipt {
            app_name: "anyprof-cli".to_string(),
        })
        .to_string();
        assert!(message.contains("release installer"), "{message}");
        assert!(message.contains("macOS and Linux:"), "{message}");
        assert!(message.contains("Windows (PowerShell):"), "{message}");
        assert!(message.contains("anyprof-cli-installer.sh"), "{message}");
        assert!(message.contains("anyprof-cli-installer.ps1"), "{message}");
        assert!(reinstall_hint().contains("releases/latest/download"));
    }

    #[test]
    fn a_failed_install_location_lookup_is_not_called_a_replace_failure() {
        let message = location_failure(AxoupdateError::Io(std::io::Error::other(
            "canonicalize failed",
        )))
        .to_string();
        assert!(message.contains("could not work out where"), "{message}");
        assert!(!message.contains("replace"), "{message}");
    }

    #[test]
    fn failed_install_names_the_exit_status_and_the_directory() {
        let message = install_failure(AxoupdateError::InstallFailed {
            status: Some(1),
            stdout: None,
            stderr: None,
        })
        .to_string();
        assert!(message.contains("could not replace"), "{message}");
        assert!(message.contains("exited with status 1"), "{message}");
        assert!(message.contains("Check that you can write to"), "{message}");
        assert!(message.contains("anyprof-cli-installer.sh"), "{message}");
        assert!(!message.contains("Output from the installer"), "{message}");
    }

    #[test]
    fn cleanup_warning_names_the_leftover_file() {
        let message = format_cleanup_warning(Path::new("anyprof.exe.previous.exe"));
        assert!(message.contains("new version is installed"), "{message}");
        assert!(message.contains("anyprof.exe.previous.exe"), "{message}");
    }
}
