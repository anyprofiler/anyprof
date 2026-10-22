//! `anyprof update`, backed by cargo-dist's [`axoupdater`] crate.
//!
//! The shell and PowerShell installers write a receipt for the `anyprof-cli`
//! app (the same name as `anyprof-cli-installer.sh`). That receipt is what
//! tells the updater which GitHub release to fetch and where the binary lives.
//! `install-updater` stays off in `dist-workspace.toml`; this subcommand is the
//! in-process updater cargo-dist documents for axoupdater-as-a-library.

use anyhow::anyhow;
use axoupdater::{AxoUpdater, AxoupdateError};

/// cargo-dist app name. The published installers are `anyprof-cli-installer.sh`
/// and `anyprof-cli-installer.ps1`.
const APP_NAME: &str = "anyprof-cli";

const SHELL_INSTALLER: &str = "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anyprofiler/anyprof/releases/latest/download/anyprof-cli-installer.sh | sh";
const POWERSHELL_INSTALLER: &str = "powershell -ExecutionPolicy Bypass -c \"irm https://github.com/anyprofiler/anyprof/releases/latest/download/anyprof-cli-installer.ps1 | iex\"";

pub fn run() -> anyhow::Result<()> {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    updater.load_receipt().map_err(install_failure)?;
    match updater
        .check_receipt_is_for_this_executable()
        .map_err(install_failure)?
    {
        true => {}
        false => {
            return Err(anyhow!(
                "this anyprof binary is not the copy installed by the GitHub release installer.\n{}",
                reinstall_hint()
            ));
        }
    }

    match updater.run_sync().map_err(install_failure)? {
        Some(update) => {
            let old = update.old_version.as_ref().map(ToString::to_string);
            println!(
                "{}",
                format_updated(old.as_deref(), &update.new_version.to_string())
            );
        }
        None => println!("{}", format_already_latest(env!("CARGO_PKG_VERSION"))),
    }
    Ok(())
}

fn format_updated(old: Option<&str>, new: &str) -> String {
    match old {
        Some(old) => format!("anyprof {old} → {new}"),
        None => format!("anyprof → {new}"),
    }
}

fn format_already_latest(version: &str) -> String {
    format!("anyprof {version} is already the latest release")
}

fn reinstall_hint() -> String {
    format!(
        "Install or reinstall with the GitHub release installer, then run `anyprof update` again:\n  {SHELL_INSTALLER}\n  {POWERSHELL_INSTALLER}"
    )
}

fn install_failure(err: AxoupdateError) -> anyhow::Error {
    match &err {
        AxoupdateError::NoReceipt { .. }
        | AxoupdateError::ReceiptLoadFailed { .. }
        | AxoupdateError::NoHome {} => anyhow!(
            "anyprof was not installed with the GitHub release installer, so it cannot update itself ({err}).\n{}",
            reinstall_hint()
        ),
        AxoupdateError::InstallFailed { .. } | AxoupdateError::Io(_) => anyhow!(
            "anyprof could not replace the installed binary ({err}).\n{}",
            reinstall_hint()
        ),
        other => anyhow!("anyprof update failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_already_latest, format_updated, install_failure, reinstall_hint};
    use axoupdater::AxoupdateError;

    #[test]
    fn update_lines_name_both_versions() {
        assert_eq!(
            format_updated(Some("0.1.0"), "0.1.1"),
            "anyprof 0.1.0 → 0.1.1"
        );
        assert_eq!(format_updated(None, "0.1.1"), "anyprof → 0.1.1");
        assert_eq!(
            format_already_latest("0.1.1"),
            "anyprof 0.1.1 is already the latest release"
        );
    }

    #[test]
    fn missing_receipt_points_at_both_installers() {
        let message = install_failure(AxoupdateError::NoReceipt {
            app_name: "anyprof-cli".to_string(),
        })
        .to_string();
        assert!(message.contains("release installer"), "{message}");
        assert!(message.contains("anyprof-cli-installer.sh"), "{message}");
        assert!(message.contains("anyprof-cli-installer.ps1"), "{message}");
        assert!(reinstall_hint().contains("releases/latest/download"));
    }
}
