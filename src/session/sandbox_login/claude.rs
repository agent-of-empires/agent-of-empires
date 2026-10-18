//! Claude's sandbox login: the `claude auth login` command, its output, and the credential
//! file it fills. Nothing outside this module knows these formats.

use regex::Regex;
use std::sync::LazyLock;

use super::{now_ms, AgentLogin, CredentialState, HomeSeed, LaunchAuth};

/// The helper's `~/.claude.json`: onboarding is skipped. The sandboxes' own seed uses the same.
pub(crate) const ONBOARDING_SEED: &str = r#"{"hasCompletedOnboarding":true}"#;

pub(crate) static LOGIN: AgentLogin = AgentLogin {
    argv: &["claude", "auth", "login"],
    home_seed: HomeSeed {
        path: ".claude.json",
        content: ONBOARDING_SEED,
    },
    credential_expires_at,
    merge_credential,
    credential_state,
    needs_credential,
    login_url: extract_login_url,
    failure_message,
    rejected_code: is_rejected_code,
};

/// Routing and credential variables that put a launch on an auth route other than the
/// shared OAuth login. aoe itself writes these flags on as `"1"` and off as the empty string,
/// never `"0"` (the adapter reads them by truthiness, for which `"0"` is on; see
/// `provider_override_env`). Claude Code's own reading is not verified here, so the hold is
/// lifted for any non-empty value: the launch is then not gated by aoe. `FOUNDRY` is from
/// Claude Code's documented environment variables and appears nowhere else in this repo.
const OTHER_AUTH_ROUTES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

fn needs_credential(auth: &LaunchAuth) -> bool {
    !OTHER_AUTH_ROUTES.iter().any(|key| auth.var(key).is_some())
}

/// The furthest `expiresAt` a real token carries. The shared file is writable
/// from inside every sandbox, so a planted timestamp beyond this would
/// otherwise outrank every later login for good.
pub(crate) const CREDENTIAL_EXPIRY_HORIZON: std::time::Duration =
    std::time::Duration::from_secs(400 * 24 * 60 * 60);

/// The `expiresAt` of a credential a container could use, or `None` when the
/// content is not one.
///
/// A blanked token block is not one. Claude Code empties `accessToken` and
/// `refreshToken` in place when its credential fails to authenticate and
/// keeps the rest of the block, so what it leaves still parses and can still
/// carry an expiry (#3860). Read as a credential, that shell mounts dead into
/// every later container and blocks the seed that would repair it.
fn credential_expires_at(content: &str, now_ms: u64) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let filled = |field| {
        oauth
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.trim().is_empty())
    };
    if !filled("accessToken") && !filled("refreshToken") {
        return None;
    }
    oauth.get("expiresAt")?.as_u64().filter(|expires_at| {
        *expires_at <= now_ms.saturating_add(CREDENTIAL_EXPIRY_HORIZON.as_millis() as u64)
    })
}

/// `winner` with everything `existing` held beside `claudeAiOauth` kept, so a
/// host copy that wins on expiry does not drop what the agent stored in the
/// file inside a container. Anything that is not two JSON objects is replaced
/// whole.
fn merge_credential(existing: Option<&str>, winner: &str) -> String {
    let parsed = |content: &str| {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(content).ok()
    };
    let merged = existing.and_then(parsed).and_then(|mut merged| {
        let oauth = parsed(winner)?.remove("claudeAiOauth")?;
        merged.insert("claudeAiOauth".to_string(), oauth);
        serde_json::to_string(&merged).ok()
    });
    merged.unwrap_or_else(|| winner.to_string())
}

fn credential_state(content: &str) -> CredentialState {
    if credential_expires_at(content, now_ms()).is_some() {
        CredentialState::Usable {
            refresh_expires_at: refresh_expires_at(content),
        }
    } else {
        CredentialState::Unusable
    }
}

/// `refreshTokenExpiresAt` when Claude writes one; its presence is unmeasured.
fn refresh_expires_at(content: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .get("claudeAiOauth")?
        .get("refreshTokenExpiresAt")?
        .as_u64()
}

/// Claude's reply to a code it does not accept (measured on 2.1.294); it does not exit afterwards.
fn is_rejected_code(output: &str) -> bool {
    output.to_ascii_lowercase().contains("invalid code")
}

static OSC8_TARGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\]8;[^;\x07\x1b]*;([^\x07\x1b]+)(?:\x07|\x1b\\)").unwrap());
static TERMINAL_CODES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[@-Z\\-_]").unwrap()
});
static SECRET_PARAMS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(state|code_challenge)=[^&\s]+").unwrap());

const MESSAGE_LINES: usize = 6;
const MESSAGE_CHARS: usize = 600;

/// Strips terminal codes and turns `\r\n` and lone `\r` into `\n`.
fn normalize(raw: &str) -> String {
    TERMINAL_CODES
        .replace_all(raw, "")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Hard wraps are only believed at common terminal widths; Claude itself prints the
/// link as one long line, and a short URL line followed by a URL-looking word must
/// not be glued together.
const WRAP_WIDTHS: [usize; 5] = [80, 100, 120, 132, 160];

fn is_login_url(url: &str) -> bool {
    (url.starts_with("https://claude.com/") || url.starts_with("https://claude.ai/"))
        && url.chars().all(is_url_char)
        && url.contains("/oauth/authorize?")
        && url.contains("state=")
        && url.contains("code_challenge=")
}

/// The URL at the start of `text`, joined across hard wraps. `prefix_width` is the
/// width of the line before the URL. A wrapped URL's physical lines all have the
/// first line's width and only the last is shorter, so a line is appended only
/// while the previous one filled that width (and the width is a common one).
fn take_wrapped_url(text: &str, prefix_width: usize) -> String {
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or_default().trim_end();
    let mut url: String = first.chars().take_while(|c| is_url_char(*c)).collect();
    if url.len() != first.len() {
        return url;
    }
    let width = prefix_width + first.chars().count();
    if !WRAP_WIDTHS.contains(&width) {
        return url;
    }
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() || line.chars().count() > width || !line.chars().all(is_url_char) {
            break;
        }
        url.push_str(line);
        if line.chars().count() < width {
            break;
        }
    }
    url
}

/// Only complete lines count: text after the last line break may still grow.
fn extract_login_url(raw: &str) -> Option<String> {
    let raw = &raw[..raw.rfind(['\n', '\r']).map_or(0, |i| i + 1)];
    if let Some(url) = OSC8_TARGET
        .captures_iter(raw)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str())
        .find(|u| is_login_url(u))
    {
        return Some(url.to_string());
    }
    let text = normalize(raw);
    let mut from = 0;
    while let Some(offset) = text[from..].find("https://") {
        let start = from + offset;
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        let prefix_width = text[line_start..start].chars().count();
        let url = take_wrapped_url(&text[start..], prefix_width);
        if is_login_url(&url) {
            return Some(url);
        }
        from = start + "https://".len();
    }
    None
}

/// Whole secrets first, then parameter values, so a URL is never half-redacted
/// into something that still carries `state`.
fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret, "<redacted>");
    }
    SECRET_PARAMS
        .replace_all(&out, "$1=<redacted>")
        .into_owned()
}

fn failure_message(output: &str, secrets: &[&str]) -> String {
    let text = redact(&normalize(output), secrets);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(MESSAGE_LINES)..].join("\n");
    if tail.is_empty() {
        return "Claude printed nothing".to_string();
    }
    let start = tail
        .char_indices()
        .rev()
        .nth(MESSAGE_CHARS - 1)
        .map_or(0, |(i, _)| i);
    tail[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // MEASURED_* = recorded on 2026-10-16 with claude 2.1.294 in aoe-sandbox:1.19.0
    // (podman), `state`/`code_challenge` replaced by SAMPLE values. measured_plain is what the
    // helper reads (no TTY). measured_pty is what `-t` prints; the helper does not use a TTY, it is
    // kept as a tolerance case. INVENTED_* / sample_wrapped / sample_osc8 / sample_cr_redraw: shapes
    // the measurement did not produce (hard wrap, link-text OSC 8, lone-CR redraw), tolerance cases only.
    const MEASURED_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload+user%3Aplugins&code_challenge=SAMPLEchallenge&code_challenge_method=S256&state=SAMPLEstate";
    const URL: &str = MEASURED_URL;

    /// Measured without a TTY (case A): plain text, `\n` line ends, the prompt has no trailing newline.
    fn measured_plain() -> String {
        format!(
            "Opening browser to sign in\u{2026}\nIf the browser didn't open, visit: {URL}\nPaste code here if prompted > "
        )
    }

    /// Measured under a pty (cases B-F, 80 and 500 columns identical, never hard-wrapped):
    /// an OSC 8 hyperlink (BEL terminated) around the same URL colored `ESC[94m ... ESC[39m`, `\r\n` ends.
    fn measured_pty() -> String {
        format!(
            "Opening browser to sign in\u{2026}\r\nIf the browser didn't open, visit: \x1b]8;;{URL}\x07\x1b[94m{URL}\x1b[39m\x1b]8;;\x07\r\nPaste code here if prompted > "
        )
    }

    fn sample_plain() -> String {
        measured_plain()
    }

    /// INVENTED tolerance case: the URL hard-wrapped at 80 columns, colored, the prompt on its own line.
    fn sample_wrapped_80_ansi() -> String {
        let line = format!("If the browser didn't open, visit: {URL}");
        let wrapped: Vec<String> = line
            .as_bytes()
            .chunks(80)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();
        format!(
            "\x1b[2mOpening browser to sign in\u{2026}\x1b[0m\r\n\x1b[36m{}\x1b[0m\r\n\x1b[1mPaste code here if prompted >\x1b[0m ",
            wrapped.join("\r\n")
        )
    }

    /// INVENTED tolerance case (link text differs from the URL).
    fn sample_osc8() -> String {
        format!(
            "If the browser didn't open, visit: \x1b]8;;{URL}\x07click here\x1b]8;;\x07\r\nPaste code here if prompted > "
        )
    }

    /// INVENTED tolerance case.
    fn sample_cr_redraw() -> String {
        format!(
            "Opening browser to sign in\u{2026}\rOpening browser to sign in\u{2026}\r\nIf the browser didn't open, visit: {URL}\r\nPaste code here if prompted > "
        )
    }

    /// INVENTED: the network-failure text was not measured.
    const INVENTED_NO_URL: &str = "Error: getaddrinfo EAI_AGAIN claude.ai\n";
    const SAMPLE_NO_URL: &str = INVENTED_NO_URL;
    /// Measured without a TTY: the error follows the prompt (which had no newline) and the client
    /// EXITS with status 1 (no re-prompt, no echo of the code).
    const MEASURED_WRONG_CODE_PLAIN: &str = "Login failed: Request failed with status code 400\n";
    /// Measured under a pty: same text, `\r\n`, cursor-show `ESC[?25h` follows. Tolerance case.
    const MEASURED_WRONG_CODE: &str =
        "Login failed: Request failed with status code 400\r\n\x1b[?25h";

    #[test]
    fn sandbox_login_parse_extracts_the_url_from_every_sample() {
        // measured_plain first: it is the one the helper sees.
        for (name, raw) in [
            ("measured_plain", measured_plain()),
            ("measured_pty", measured_pty()),
            ("wrapped", sample_wrapped_80_ansi()),
            ("osc8", sample_osc8()),
            ("cr", sample_cr_redraw()),
        ] {
            assert_eq!(extract_login_url(&raw).as_deref(), Some(URL), "{name}");
        }
        assert_eq!(extract_login_url(SAMPLE_NO_URL), None);
        assert_eq!(
            extract_login_url("visit: http://claude.ai/oauth/authorize?state=x&code_challenge=y"),
            None,
            "not https"
        );
        assert_eq!(
            extract_login_url("visit: https://claude.ai/oauth/authorize?code=true"),
            None,
            "no state/challenge"
        );
        // A URL split across reads is found once the second half arrives.
        let full = sample_plain();
        let cut = full.find("state=").unwrap();
        assert_eq!(extract_login_url(&full[..cut]), None);
    }

    #[test]
    fn sandbox_login_parse_measured_plain_sample() {
        assert_eq!(extract_login_url(&measured_plain()).as_deref(), Some(URL));
    }

    #[test]
    fn sandbox_login_parse_rejects_foreign_hosts() {
        assert_eq!(
            extract_login_url(
                "visit: https://evil.example/oauth/authorize?state=1&code_challenge=2\n"
            ),
            None
        );
        assert_eq!(
            extract_login_url(
                "visit: \x1b]8;;https://evil.example/oauth/authorize?state=1&code_challenge=2\x07x\x1b]8;;\x07\n"
            ),
            None
        );
        assert_eq!(
            extract_login_url(&format!("visit: \x1b]8;;{URL}\u{7}\u{1b}[0m x\n")),
            Some(URL.to_string()),
            "a clean OSC 8 target is accepted"
        );
        assert_eq!(
            extract_login_url(
                "visit: \x1b]8;;https://claude.com/oauth/authorize?state=1&code_challenge=2 x\x07t\x1b]8;;\x07\n"
            ),
            None,
            "OSC 8 target with a non-URL character"
        );
    }

    #[test]
    fn sandbox_login_parse_only_complete_lines() {
        let full = sample_plain();
        let cut = full.find("state=SAMPLEst").unwrap() + "state=SAMPLEst".len();
        assert_eq!(extract_login_url(&full[..cut]), None);
        assert!(extract_login_url(&format!("{}\n", &full[..cut])).is_some());
    }

    #[test]
    fn sandbox_login_parse_line_after_the_url_is_not_joined() {
        let out = format!(
            "If the browser didn't open, visit: {URL}\nWaiting\nPaste code here if prompted > "
        );
        assert_eq!(extract_login_url(&out).as_deref(), Some(URL));
        // Also for a short URL line followed by a URL-looking word.
        let short = "https://claude.com/oauth/authorize?state=a&code_challenge=b";
        let out = format!("{short}\nWaiting\n");
        assert_eq!(extract_login_url(&out).as_deref(), Some(short));
    }

    #[test]
    fn sandbox_login_parse_redact_hides_url_parameters_on_their_own() {
        let redacted = redact(URL, &[]);
        assert!(!redacted.contains("SAMPLEstate"), "{redacted}");
        assert!(!redacted.contains("SAMPLEchallenge"), "{redacted}");
    }

    #[test]
    fn sandbox_login_parse_failure_message_redacts_code_url_and_state() {
        let code = "AAAAfakecodeAAAA#BBBBfakestateBBBB";
        // Primary: plain output. Tolerance: pty output. Defensive: the code printed back.
        for out in [
            format!("{}{}", measured_plain(), MEASURED_WRONG_CODE_PLAIN),
            format!("{}{}", measured_pty(), MEASURED_WRONG_CODE),
            format!("{}{code}\n{}", measured_plain(), MEASURED_WRONG_CODE_PLAIN),
        ] {
            let msg = failure_message(&out, &[code, URL]);
            assert!(
                msg.contains("Login failed: Request failed with status code 400"),
                "{msg}"
            );
            for leak in [code, "SAMPLEstate", "SAMPLEchallenge", URL] {
                assert!(!msg.contains(leak), "{leak} leaked: {msg}");
            }
        }
        assert!(failure_message(&"x\n".repeat(5000), &[]).len() <= 600);
        assert_eq!(failure_message("", &[]), "Claude printed nothing");
    }

    fn auth(env: &[(&str, &str)]) -> LaunchAuth {
        LaunchAuth::from_env(
            &env.iter()
                .map(|(key, value)| crate::containers::EnvEntry::Literal {
                    key: key.to_string(),
                    value: value.to_string(),
                })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn only_a_launch_on_the_oauth_route_needs_the_shared_credential() {
        let cases: [(&[(&str, &str)], bool); 11] = [
            (&[], true),
            (&[("CLAUDE_CODE_USE_BEDROCK", "")], true),
            (
                &[
                    ("CLAUDE_CODE_USE_BEDROCK", "1"),
                    ("CLAUDE_CODE_USE_VERTEX", ""),
                ],
                false,
            ),
            (&[("CLAUDE_CODE_USE_VERTEX", "1")], false),
            (&[("CLAUDE_CODE_USE_FOUNDRY", "1")], false),
            (&[("CLAUDE_CODE_USE_BEDROCK", "0")], false),
            (&[("ANTHROPIC_API_KEY", "sk-test")], false),
            (&[("ANTHROPIC_API_KEY", "")], true),
            (&[("ANTHROPIC_AUTH_TOKEN", "token")], false),
            (&[("CLAUDE_CODE_OAUTH_TOKEN", "token")], false),
            (&[("AWS_PROFILE", "dev")], true),
        ];
        for (env, needs) in cases {
            assert_eq!((LOGIN.needs_credential)(&auth(env)), needs, "{env:?}");
        }
    }

    #[test]
    fn credential_state_reads_usability_and_refresh_expiry() {
        let expires_at = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let usable = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":"r","expiresAt":{expires_at},"refreshTokenExpiresAt":1790000000000}}}}"#
        );
        let no_refresh_expiry = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":"r","expiresAt":{expires_at}}}}}"#
        );
        let blanked = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"","refreshToken":"","expiresAt":{expires_at}}}}}"#
        );
        for (content, expected) in [
            (
                Some(usable.as_str()),
                CredentialState::Usable {
                    refresh_expires_at: Some(1_790_000_000_000),
                },
            ),
            (
                Some(no_refresh_expiry.as_str()),
                CredentialState::Usable {
                    refresh_expires_at: None,
                },
            ),
            (Some(blanked.as_str()), CredentialState::Unusable),
            (Some("{}"), CredentialState::Unusable),
            (Some("not json"), CredentialState::Unusable),
            (None, CredentialState::Unusable),
        ] {
            assert_eq!(LOGIN.state(content), expected, "{content:?}");
        }
    }

    #[test]
    fn a_rejected_code_is_recognized_in_the_clients_reply() {
        assert!((LOGIN.rejected_code)(
            "Invalid code. Please make sure the full code was copied.\n"
        ));
        assert!((LOGIN.rejected_code)("INVALID CODE"));
        assert!(!(LOGIN.rejected_code)(
            "Login failed: Request failed with status code 400"
        ));
    }

    fn credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"access-{expires_at}","expiresAt":{expires_at},"refreshToken":"refresh-{expires_at}"}}}}"#
        )
    }

    fn blanked_credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"","expiresAt":{expires_at},"refreshToken":"","scopes":["user:inference"],"subscriptionType":"max"}}}}"#
        )
    }

    #[test]
    fn only_a_usable_credential_carries_an_expiry() {
        let now = now_ms();
        let horizon = CREDENTIAL_EXPIRY_HORIZON.as_millis() as u64;
        let cases = [
            (credential(1700000000), Some(1700000000)),
            // Emptied tokens are not a credential, whatever expiry is left
            // beside them (#3860).
            (blanked_credential(1700000000), None),
            (blanked_credential(0), None),
            // Either token alone keeps a container going: an access token
            // until it expires, a refresh token to get another.
            (
                r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":10}}"#.into(),
                Some(10),
            ),
            (
                r#"{"claudeAiOauth":{"refreshToken":"r","expiresAt":10}}"#.into(),
                Some(10),
            ),
            (r#"{"other":"data"}"#.into(), None),
            (r#"{"claudeAiOauth":{"accessToken":"a"}}"#.into(), None),
            (
                r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":"10"}}"#.into(),
                None,
            ),
            ("not json at all".into(), None),
            (String::new(), None),
            // Planted from inside a sandbox to outrank every later login.
            (credential(now + horizon + 1), None),
        ];
        for (content, expires_at) in cases {
            assert_eq!(
                credential_expires_at(&content, now),
                expires_at,
                "{content}"
            );
        }
    }

    #[test]
    fn merge_keeps_what_the_file_held_beside_the_oauth_token() {
        let existing = r#"{"claudeAiOauth":{"expiresAt":1},"designOauth":{"k":"v"}}"#;
        let winner = r#"{"claudeAiOauth":{"expiresAt":2}}"#;
        let merged: serde_json::Value =
            serde_json::from_str(&merge_credential(Some(existing), winner)).unwrap();
        assert_eq!(merged["claudeAiOauth"]["expiresAt"], 2);
        assert_eq!(merged["designOauth"]["k"], "v");
        assert_eq!(merge_credential(Some("{}"), winner), winner);
        assert_eq!(merge_credential(Some("not json"), winner), winner);
        assert_eq!(merge_credential(None, winner), winner);
    }
}

/// Credential files in the native format, for the tests of everything that fills or reads one.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Fields in the order `serde_json` writes them back, so a fold that re-serializes a
    /// credential produces this string again.
    pub(crate) fn credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"access-{expires_at}","expiresAt":{expires_at},"refreshToken":"refresh-{expires_at}"}}}}"#
        )
    }

    /// What Claude Code leaves behind when its credential fails to authenticate: both tokens
    /// emptied in place, the rest of the block kept.
    pub(crate) fn blanked_credential(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"","expiresAt":{expires_at},"refreshToken":"","scopes":["user:inference"],"subscriptionType":"max"}}}}"#
        )
    }

    /// A login as the sandbox client writes it.
    pub(crate) fn usable(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":{expires_at}}}}}"#
        )
    }

    pub(crate) fn refresh_only(expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"","refreshToken":"fake-refresh","expiresAt":{expires_at}}}}}"#
        )
    }

    pub(crate) fn with_refresh_expiry(expires_at: u64, refresh_expires_at: u64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":{expires_at},"refreshTokenExpiresAt":{refresh_expires_at}}}}}"#
        )
    }

    /// Written in an order and spacing a re-serializing merge would change, with data of
    /// another client beside the login.
    pub(crate) fn beside_other_data() -> String {
        format!(
            "{{ \"designOauth\": {{\"k\": \"v\"}}, \"claudeAiOauth\": {} }}\n",
            r#"{"refreshToken":"refresh-a","accessToken":"access-a","expiresAt":1000}"#
        )
    }
}
