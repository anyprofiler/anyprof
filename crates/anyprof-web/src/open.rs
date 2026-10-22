use std::ffi::OsString;

/// What [`open_browser`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// The default browser was asked to open the URL.
    Opened,
    /// No browser was started. The reason is a short phrase for the user.
    Skipped(&'static str),
    /// Starting the browser failed.
    Failed(String),
}

/// Open `url` in the default browser unless that cannot work here.
///
/// Skips when `no_open` is set, inside an SSH session (`SSH_CONNECTION`), and
/// on Linux and the BSDs when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set
/// (WSL is exempt, since it opens the Windows browser).
pub fn open_browser(url: &str, no_open: bool) -> Launch {
    if let Some(reason) = skip_reason(no_open, NEEDS_DISPLAY, &|key| std::env::var_os(key)) {
        return Launch::Skipped(reason);
    }
    match webbrowser::open(url) {
        Ok(()) => Launch::Opened,
        Err(err) => Launch::Failed(err.to_string()),
    }
}

/// The command that forwards the viewer's port from another machine.
#[must_use]
pub fn ssh_forward_hint(port: u16) -> String {
    format!("ssh -L {port}:127.0.0.1:{port} <user>@<this-host>")
}

const NEEDS_DISPLAY: bool = cfg!(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "ios"),
    not(target_os = "android")
));

fn skip_reason(
    no_open: bool,
    needs_display: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<&'static str> {
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    if no_open {
        return Some("--no-open was passed");
    }
    if set("SSH_CONNECTION") {
        return Some("this is an SSH session (SSH_CONNECTION is set)");
    }
    if needs_display && !set("DISPLAY") && !set("WAYLAND_DISPLAY") && !set("WSL_DISTRO_NAME") {
        return Some("neither DISPLAY nor WAYLAND_DISPLAY is set");
    }
    None
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{skip_reason, ssh_forward_hint};

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn no_open_and_ssh_always_skip() {
        assert_eq!(
            skip_reason(true, false, &env(&[])),
            Some("--no-open was passed")
        );
        let ssh = env(&[
            ("SSH_CONNECTION", "10.0.0.1 5000 10.0.0.2 22"),
            ("DISPLAY", ":0"),
        ]);
        assert!(skip_reason(false, false, &ssh).unwrap().contains("SSH"));
        assert!(skip_reason(false, true, &ssh).unwrap().contains("SSH"));
    }

    #[test]
    fn linux_needs_a_display_unless_wsl() {
        assert!(
            skip_reason(false, true, &env(&[]))
                .unwrap()
                .contains("DISPLAY")
        );
        assert!(
            skip_reason(false, true, &env(&[("DISPLAY", "")]))
                .unwrap()
                .contains("DISPLAY")
        );
        assert_eq!(skip_reason(false, true, &env(&[("DISPLAY", ":0")])), None);
        assert_eq!(
            skip_reason(false, true, &env(&[("WAYLAND_DISPLAY", "wayland-0")])),
            None
        );
        assert_eq!(
            skip_reason(false, true, &env(&[("WSL_DISTRO_NAME", "Ubuntu")])),
            None
        );
        assert_eq!(skip_reason(false, false, &env(&[])), None);
        assert_eq!(
            skip_reason(false, false, &env(&[("SSH_CONNECTION", "")])),
            None
        );
    }

    #[test]
    fn hint_forwards_the_same_port() {
        assert_eq!(
            ssh_forward_hint(51234),
            "ssh -L 51234:127.0.0.1:51234 <user>@<this-host>"
        );
    }
}
