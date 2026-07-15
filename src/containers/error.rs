use thiserror::Error;

/// Errors from Docker / Podman / Apple Container operations.
///
/// `Display` strings are single-line by convention: the three unit variants
/// (`NotInstalled`, `DaemonNotRunning`, `PermissionDenied`) inline their
/// actionable remediation on one line, and the string-carrying variants
/// (`InspectFailed`, `RemoveFailed`, etc.) must be constructed via
/// `sanitize_stderr` to normalize any embedded newlines from raw runtime
/// stderr. This lets `tracing::warn!(error = %e)` at gate sites emit one
/// physical log line, keeping `grep target` correlation intact under the
/// text-mode subscriber this project uses.
#[derive(Debug, Error)]
pub enum DockerError {
    #[error(
        "Docker is not installed or not in PATH. Install: https://docs.docker.com/get-docker/"
    )]
    NotInstalled,

    #[error(
        "Docker daemon is not running. Start Docker Desktop or run: sudo systemctl start docker"
    )]
    DaemonNotRunning,

    #[error("Docker permission denied. On Linux: add your user to the docker group (sudo usermod -aG docker $USER) and re-login")]
    PermissionDenied,

    #[error("Container not found: {0}")]
    ContainerNotFound(String),

    #[error("Container already exists: {0}")]
    ContainerAlreadyExists(String),

    #[error("Docker image not found: {0}")]
    ImageNotFound(String),

    #[error("Failed to create container: {0}")]
    CreateFailed(String),

    #[error("Failed to start container: {0}")]
    StartFailed(String),

    #[error("Failed to stop container: {0}")]
    StopFailed(String),

    #[error("Failed to remove container: {0}")]
    RemoveFailed(String),

    #[error("Failed to inspect container: {0}")]
    InspectFailed(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Collapse embedded newlines and trim so a raw runtime stderr string,
/// when wrapped in a [`DockerError`] string-carrying variant, still
/// renders as a single-line `Display`.
///
/// Docker / Podman / Apple stderr routinely contains `\n` between error
/// summary lines. Without this, `tracing::warn!(error = %e)` on a gate
/// site would split one logical event across physical log lines,
/// re-introducing the grep-hostility the single-line `Display`
/// convention fixed for the unit variants. Call at every construction
/// site that wraps stderr into a `DockerError` variant.
///
/// Handles Unix (`\n`) and Windows (`\r\n`) line terminators via
/// [`str::lines`], and skips whitespace-only lines so blank interior
/// separators do not become empty ` | ` segments.
///
/// When the input is empty or entirely whitespace, returns the sentinel
/// `"<no stderr>"` rather than an empty string. A runtime CLI can exit
/// non-zero without writing to stderr, and an empty argument would render a
/// string-carrying variant such as `InspectFailed("")` as a dangling
/// `"Failed to inspect container: "` with no operator signal; the sentinel
/// keeps the Display line self-describing.
pub(crate) fn sanitize_stderr(stderr: &str) -> String {
    let joined = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.is_empty() {
        return "<no stderr>".to_string();
    }
    joined
}

pub type Result<T> = std::result::Result<T, DockerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_are_single_line_and_actionable() {
        for e in [
            DockerError::NotInstalled,
            DockerError::DaemonNotRunning,
            DockerError::PermissionDenied,
        ] {
            let s = e.to_string();
            assert!(!s.contains('\n'), "Display must be single-line: {s:?}");
            assert!(s.len() < 160, "Display should fit terminal-wrap: {s:?}");
        }
        assert!(DockerError::NotInstalled
            .to_string()
            .contains("docs.docker.com"));
        assert!(DockerError::DaemonNotRunning
            .to_string()
            .contains("systemctl"));
        assert!(DockerError::PermissionDenied
            .to_string()
            .contains("usermod"));
    }

    #[test]
    fn parameterized_variants_stay_single_line_when_stderr_multiline() {
        let raw = "Error response from daemon: something failed\nAdditional context: line two\n";
        let sanitized = sanitize_stderr(raw);
        let e = DockerError::InspectFailed(sanitized);
        let rendered = e.to_string();
        assert!(
            !rendered.contains('\n'),
            "sanitize_stderr must strip newlines from parameterized variant Display: {rendered:?}"
        );
        assert!(
            rendered.contains(" | "),
            "sanitize_stderr should join lines with ` | ` for readability: {rendered:?}"
        );
    }

    #[test]
    fn sanitize_stderr_trims_trailing_whitespace() {
        assert_eq!(sanitize_stderr("some error\n"), "some error");
        assert_eq!(sanitize_stderr("  padded  \n"), "padded");
        assert_eq!(sanitize_stderr("a\nb\nc"), "a | b | c");
    }

    #[test]
    fn sanitize_stderr_handles_empty_and_whitespace_inputs() {
        // Empty / whitespace-only stderr collapses to the `<no stderr>`
        // sentinel so a string-carrying variant never renders a dangling
        // colon with no operator signal.
        assert_eq!(sanitize_stderr(""), "<no stderr>");
        assert_eq!(sanitize_stderr("   "), "<no stderr>");
        assert_eq!(sanitize_stderr("\n\n\n"), "<no stderr>");
    }

    #[test]
    fn empty_stderr_variant_display_has_no_dangling_colon() {
        // A runtime CLI that exits non-zero without writing to stderr must
        // still produce a self-describing Display, not a bare trailing
        // "Failed to inspect container: ".
        let e = DockerError::InspectFailed(sanitize_stderr(""));
        assert_eq!(e.to_string(), "Failed to inspect container: <no stderr>");
    }

    #[test]
    fn sanitize_stderr_handles_crlf_line_endings() {
        // `str::lines` splits on `\n` and `\r\n`, so Windows-style
        // stderr does not leave a bare `\r` mid-string that would
        // corrupt the terminal render.
        assert_eq!(sanitize_stderr("a\r\nb"), "a | b");
        assert_eq!(
            sanitize_stderr("first\r\nsecond\r\nthird"),
            "first | second | third"
        );
    }

    #[test]
    fn sanitize_stderr_skips_blank_interior_lines() {
        // Docker's error responses occasionally include blank lines
        // between summary and detail. Filtering them out keeps the
        // rendered string readable without empty ` | ` segments.
        assert_eq!(sanitize_stderr("line1\n\nline2"), "line1 | line2");
        assert_eq!(sanitize_stderr("a\n  \nb"), "a | b");
    }

    #[test]
    fn sanitize_stderr_is_idempotent() {
        // Applying sanitize_stderr twice must produce the same result
        // as applying it once. Protects against a future helper that
        // accidentally double-sanitizes and produces ` |  | ` sequences.
        let once = sanitize_stderr("a\nb\nc");
        let twice = sanitize_stderr(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn sanitize_stderr_lone_cr_is_preserved_interior() {
        // str::lines only treats \n and \r\n as terminators (per Rust
        // spec), so a lone \r mid-string is NOT a line boundary and
        // survives str::trim, which strips whitespace only at boundaries.
        // Docker/Podman/Apple do not emit lone \r in stderr, so this test
        // documents the known limitation rather than exercising a
        // real-world path: if a lone \r ever surfaces, it will appear
        // verbatim in the Display output.
        assert_eq!(sanitize_stderr("a\rb"), "a\rb");
    }
}
