//! The single seam for opening a URL in the user's browser.
//!
//! Every open goes through [`open_url`] so there is one place to intercept and
//! one set of reachability rules. `aoe serve --open` routes through it too, so
//! the server and the TUI cannot disagree about whether a browser is reachable
//! on the same host.
//! When `AOE_OPEN_URL_TO` names a file, the URL is appended to it (one per
//! line) instead of launching a browser; a live-daemon e2e sets it so it can
//! assert the exact URL a chord resolved without spawning a real browser.
//! Unset in normal use, so production behavior is unchanged.

use std::io::Write;

/// Test hook: a file to append opened URLs to instead of launching a browser.
/// Unset in normal runs.
const OPEN_URL_TO_ENV: &str = "AOE_OPEN_URL_TO";

/// Open `url` in the user's browser, or, when `AOE_OPEN_URL_TO` is set, append
/// it to that file instead. Errors propagate so the caller can toast a failure.
///
/// Refuses up front when no browser this user could see is reachable, rather
/// than reporting a success that never happens: `webbrowser::open` returns as
/// soon as it can spawn a helper, so a headless host reports `Ok` while the
/// spawned `xdg-open` fails out of band, and over SSH a browser that does start
/// opens on a screen the user is not sitting at.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if let Ok(path) = std::env::var(OPEN_URL_TO_ENV) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{url}")?;
        return Ok(());
    }
    if !browser_reachable() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no browser you could see from here",
        ));
    }
    webbrowser::open(url)
}

/// Whether launching a browser here would put it in front of the user.
///
/// `BROWSER` is honored first and unconditionally: setting it is a deliberate
/// choice, and on a remote host it is usually a script that forwards the URL
/// somewhere useful.
fn browser_reachable() -> bool {
    if std::env::var_os("BROWSER").is_some() {
        return true;
    }
    // In an SSH session the user is elsewhere. X11 forwarding is the exception:
    // it puts the display back on their own machine.
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return std::env::var_os("DISPLAY").is_some();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // A bare Linux server has an `xdg-open` that spawns fine and then fails
        // for want of a display.
        std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WSL_DISTRO_NAME").is_some()
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Save and clear every var `browser_reachable` reads, restoring on drop so
    /// a panicking assert cannot leak a cleared `DISPLAY` into sibling tests.
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn clear_all() -> Self {
            const KEYS: [&str; 5] = [
                "BROWSER",
                "SSH_CONNECTION",
                "SSH_TTY",
                "DISPLAY",
                "WAYLAND_DISPLAY",
            ];
            let saved = KEYS
                .iter()
                .map(|k| {
                    let prev = std::env::var_os(k);
                    std::env::remove_var(k);
                    (*k, prev)
                })
                .collect();
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// `webbrowser::open` returns as soon as it can spawn a helper, so an
    /// unreachable browser used to be reported as a successful open while
    /// nothing happened. The caller shows the user what actually occurred, so
    /// this has to refuse rather than optimistically succeed.
    #[test]
    #[serial]
    fn refuses_when_no_browser_could_reach_the_user() {
        let _guard = EnvGuard::clear_all();

        // Over SSH the user is at another machine entirely.
        std::env::set_var("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 22");
        assert!(
            !browser_reachable(),
            "ssh with no display cannot reach them"
        );

        // ...unless X11 forwarding puts the display back on their machine.
        std::env::set_var("DISPLAY", "localhost:10.0");
        assert!(browser_reachable(), "forwarded display reaches them");
        std::env::remove_var("DISPLAY");

        // An explicit BROWSER is a deliberate choice; honour it either way.
        std::env::set_var("BROWSER", "my-forwarder");
        assert!(browser_reachable(), "an explicit BROWSER always wins");
    }

    /// The refusal must be an error, so the caller falls back rather than
    /// telling the user a link opened.
    #[test]
    #[serial]
    fn unreachable_browser_is_an_error_not_a_silent_success() {
        let _guard = EnvGuard::clear_all();
        std::env::set_var("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 22");
        // No AOE_OPEN_URL_TO, so this takes the real path; it must refuse
        // before reaching `webbrowser`, which would spawn something.
        let err = open_url("https://example.com").expect_err("must not report success");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    #[serial]
    fn redirect_appends_each_url_when_env_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opened.txt");
        std::env::set_var(OPEN_URL_TO_ENV, &path);
        open_url("https://example.com/pr/1").unwrap();
        open_url("https://example.com/pr/2").unwrap();
        std::env::remove_var(OPEN_URL_TO_ENV);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "https://example.com/pr/1\nhttps://example.com/pr/2\n"
        );
    }
}
