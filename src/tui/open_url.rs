//! The single seam the TUI uses to open a URL in the user's browser.
//!
//! Every open goes through [`open_url`] so there is one place to intercept.
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
pub fn open_url(url: &str) -> std::io::Result<()> {
    if let Ok(path) = std::env::var(OPEN_URL_TO_ENV) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{url}")?;
        return Ok(());
    }
    webbrowser::open(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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
