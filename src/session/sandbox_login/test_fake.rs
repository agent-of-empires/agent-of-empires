//! A fake `claude auth login` helper process for tests of the helper and its callers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use super::claude::LOGIN;
use super::helper::{HelperTimeouts, LoginError, LoginHelper};

// Measured shape (Task 0), `state`/`code_challenge` replaced by SAMPLE values.
pub(crate) const MEASURED_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload+user%3Aplugins&code_challenge=SAMPLEchallenge&code_challenge_method=S256&state=SAMPLEstate";
pub(crate) const DEADLINE: Duration = Duration::from_secs(10);

/// Imitates the measured plain output: the URL line over two writes, the prompt
/// without a newline, one `read`. `HANG_ON_EOF=1` ignores EOF like the real client.
const FAKE_HELPER: &str = r#"#!/bin/sh
printf 'Opening browser to sign in\342\200\246\n'
half=${#URL}; half=$((half / 2))
printf "If the browser didn't open, visit: %s" "$(printf %s "$URL" | cut -c1-$half)"
printf '%s\n' "$(printf %s "$URL" | cut -c$((half + 1))-)"
printf 'Paste code here if prompted > '
if ! IFS= read -r code; then
  [ -n "$HANG_ON_EOF" ] && exec sleep 3600
  exit 3
fi
case "$code" in
  good-code#good-state)
    printf '{"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":%s}}' "$(( $(date +%s) * 1000 + 3600000 ))" > "$CRED"
    printf 'Login successful.\n'; exit 0 ;;
  lie*) printf 'Login successful.\n'; exit 0 ;;
  late*)
    printf '{"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":%s}}' "$(( $(date +%s) * 1000 + 3600000 ))" > "$CRED"
    exec sleep 3600 ;;
  hang*) exec sleep 3600 ;;
  invalid*) printf 'Invalid code. Please make sure the full code was copied.\n'; exec sleep 3600 ;;
  echo*) printf '%s\n%s\n' "$code" "$URL"; printf 'Login failed: Request failed with status code 400\n' >&2; exit 1 ;;
  *) printf 'Login failed: Request failed with status code 400\n'; exit 1 ;;
esac
"#;

pub(crate) struct Fake {
    pub(crate) tmp: TempDir,
    pub(crate) shared: PathBuf,
}

impl Fake {
    pub(crate) fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let shared = tmp.path().join("sandbox-v2").join(".credentials.json");
        Self::at(tmp, shared)
    }

    /// A fake logging into `shared`, which a test chose (for example the isolated home's
    /// real shared credential file); `tmp` holds the script.
    pub(crate) fn at(tmp: TempDir, shared: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let script = tmp.path().join("fake.sh");
        std::fs::write(&script, FAKE_HELPER).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, "{}").unwrap();
        Self { tmp, shared }
    }

    pub(crate) fn argv(&self, hang_on_eof: bool) -> Vec<String> {
        let mut argv = vec![
            "/usr/bin/env".to_string(),
            format!("URL={MEASURED_URL}"),
            format!("CRED={}", self.shared.display()),
        ];
        if hang_on_eof {
            argv.push("HANG_ON_EOF=1".to_string());
        }
        argv.push("/bin/sh".to_string());
        argv.push(self.tmp.path().join("fake.sh").display().to_string());
        argv
    }

    pub(crate) fn cleaned(&self) -> PathBuf {
        self.tmp.path().join("cleaned")
    }

    pub(crate) fn cleanup(&self) -> Option<Vec<String>> {
        Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("touch '{}'", self.cleaned().display()),
        ])
    }

    pub(crate) async fn start(&self, hang_on_eof: bool) -> Result<Arc<LoginHelper>, LoginError> {
        self.start_timed(hang_on_eof, HelperTimeouts::default())
            .await
    }

    pub(crate) async fn start_timed(
        &self,
        hang_on_eof: bool,
        timeouts: HelperTimeouts,
    ) -> Result<Arc<LoginHelper>, LoginError> {
        bounded(LoginHelper::start_with(
            &LOGIN,
            self.argv(hang_on_eof),
            self.shared.clone(),
            self.cleanup(),
            timeouts,
        ))
        .await
    }
}

pub(crate) async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("deadline exceeded")
}
