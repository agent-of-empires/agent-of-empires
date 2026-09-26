use thiserror::Error;

/// `Display` is single-line: string-carrying variants must be built via `sanitize_stderr`.
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

/// Joins non-blank stderr lines with ` | `; empty input becomes `<no stderr>`.
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
    fn empty_stderr_variant_display_has_no_dangling_colon() {
        let e = DockerError::InspectFailed(sanitize_stderr(""));
        assert_eq!(e.to_string(), "Failed to inspect container: <no stderr>");
    }

    #[test]
    fn sanitize_stderr_joins_trimmed_nonblank_lines() {
        for (raw, expected) in [
            ("some error\n", "some error"),
            ("  padded  \n", "padded"),
            ("a\nb\nc", "a | b | c"),
            ("", "<no stderr>"),
            ("   ", "<no stderr>"),
            ("\n\n\n", "<no stderr>"),
            ("a\r\nb", "a | b"),
            ("first\r\nsecond\r\nthird", "first | second | third"),
            ("line1\n\nline2", "line1 | line2"),
            ("a\n  \nb", "a | b"),
            // A lone `\r` is not a line terminator and survives verbatim.
            ("a\rb", "a\rb"),
        ] {
            let once = sanitize_stderr(raw);
            assert_eq!(once, expected, "{raw:?}");
            assert_eq!(sanitize_stderr(&once), once, "idempotent for {raw:?}");
        }
    }
}
