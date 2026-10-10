use super::ReadFailure;
use std::ffi::{OsStr, OsString};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub(crate) const TOKEN_ENV: &str = "AOE_DAEMON_TOKEN";
pub(crate) const URL_ENV: &str = "AOE_DAEMON_URL";
pub(crate) const PROFILE_ENV: &str = "AGENT_OF_EMPIRES_PROFILE";

#[derive(Debug, Clone)]
pub struct ReadRequestSource {
    pub explicit_url: Option<String>,
    pub env_url: Option<OsString>,
    pub token: Option<OsString>,
    pub explicit_profile: Option<String>,
    pub env_profile: Option<OsString>,
}

/// What makes an endpoint URL unset: absent, empty, or whitespace only. A
/// shell profile that exports `AOE_DAEMON_URL` empty has expressed no
/// endpoint, and an empty one has always selected the local transport, so the
/// served half may not invent a stricter rule than the local half has. The URL
/// is compared after trimming because a value that is only whitespace is
/// unreadable as a URL either way.
pub(crate) fn url_is_unset(text: &str) -> bool {
    text.trim().is_empty()
}

/// [`url_is_unset`] for a raw environment value. A value that is not UTF-8 is
/// not unset: it is a malformed endpoint, and it fails as one.
pub(crate) fn env_url_is_unset(value: &OsStr) -> bool {
    value.to_str().is_some_and(url_is_unset)
}

impl ReadRequestSource {
    /// Whether the environment names an endpoint. Read through
    /// `env_url_is_unset`, which is crate-private, so a source built by hand
    /// and one read from the process agree on what "unset" means.
    pub fn env_url_is_set(&self) -> bool {
        self.env_url
            .as_deref()
            .is_some_and(|value| !env_url_is_unset(value))
    }
}

pub(crate) fn read_request_source(cli: &super::Cli) -> ReadRequestSource {
    ReadRequestSource {
        explicit_url: cli.daemon_url.clone(),
        env_url: std::env::var_os(URL_ENV).filter(|value| !env_url_is_unset(value)),
        token: std::env::var_os(TOKEN_ENV),
        explicit_profile: cli.profile.clone(),
        // Captured raw on purpose: this is the only record that the user named
        // a profile at all, and `main` derives `profile_explicit`: the flag
        // that decides whether `aoe project add` writes to the profile or the
        // global registry, from the value that ends up in `cli.profile`. An
        // empty variable is "no selection" for a read
        // (`selected_profile_source`), which is not the same statement as "the
        // user asked for no profile in particular" on a write path.
        env_profile: std::env::var_os(PROFILE_ENV),
    }
}

#[derive(Debug)]
pub(crate) enum SelectedEndpoint {
    Local,
    Http {
        /// Boxed: the local branch carries no data, so the enum is as wide as
        /// the request it has to hold or it is wider than every other value
        /// this function returns.
        request: Box<tokio_tungstenite::tungstenite::handshake::client::Request>,
    },
}

pub(crate) fn select_endpoint(source: &ReadRequestSource) -> Result<SelectedEndpoint, ReadFailure> {
    let selected = match (&source.explicit_url, &source.env_url) {
        (Some(_), _) => source.explicit_url.as_deref(),
        (None, Some(value)) => {
            let value = value
                .to_str()
                .ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?;
            if url_is_unset(value) {
                return Ok(SelectedEndpoint::Local);
            }
            Some(value.trim())
        }
        (None, None) => return Ok(SelectedEndpoint::Local),
    };
    let raw = selected.ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?;
    let (request_url, secure) = parse_endpoint(raw)?;
    let mut request = request_url
        .into_client_request()
        .map_err(|_| ReadFailure::pre("invalid_endpoint"))?;
    if let Some(token) = source.token.as_deref() {
        let value = valid_token_bytes(token).ok_or_else(|| ReadFailure::pre("invalid_token"))?;
        let value = std::str::from_utf8(value).expect("validated ASCII");
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {value}")
                .parse()
                .map_err(|_| ReadFailure::pre("invalid_token"))?,
        );
    }
    debug_assert_eq!(secure, request.uri().scheme_str() == Some("wss"));
    Ok(SelectedEndpoint::Http {
        request: Box::new(request),
    })
}

/// An explicit flag wins even when empty. Empty inputs select the default; whitespace remains a name.
pub(crate) fn selected_profile_source(source: &ReadRequestSource) -> ProfileSource<'_> {
    if let Some(value) = source.explicit_profile.as_deref() {
        return if value.is_empty() {
            ProfileSource::Default
        } else {
            ProfileSource::Explicit(value)
        };
    }
    if let Some(value) = source.env_profile.as_deref() {
        return if value.is_empty() {
            ProfileSource::Default
        } else {
            ProfileSource::Environment(value)
        };
    }
    ProfileSource::Default
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProfileSource<'a> {
    Explicit(&'a str),
    Environment(&'a OsStr),
    Default,
}

fn valid_token_bytes(token: &OsStr) -> Option<&[u8]> {
    let bytes = token.as_encoded_bytes();

    (!bytes.is_empty()
        && bytes.len() <= 4096
        && bytes
            .iter()
            .all(|b| (0x21..=0x7e).contains(b) && *b != b'"' && *b != b'\\'))
    .then_some(bytes)
}

fn parse_endpoint(raw: &str) -> Result<(String, bool), ReadFailure> {
    let invalid = || ReadFailure::pre("invalid_endpoint");
    if raw.is_empty()
        || raw.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
        || raw.chars().any(char::is_control)
    {
        return Err(invalid());
    }

    let (rest, secure) = if raw
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        (&raw[7..], false)
    } else if raw
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        (&raw[8..], true)
    } else {
        return Err(invalid());
    };

    let (authority, raw_path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(invalid());
    }
    let (host, port): (&str, Option<u16>) = split_authority(authority).ok_or_else(invalid)?;
    // The connector pins localhost to loopback IPs before sending plaintext credentials.
    if !secure && !crate::daemon::is_loopback_host(host) {
        return Err(invalid());
    }
    if secure && !valid_https_host(host) {
        return Err(invalid());
    }

    let path = validate_path(raw_path).ok_or_else(invalid)?;
    let ws_scheme = if secure { "wss" } else { "ws" };
    let explicit_port = port.map(|value| format!(":{value}")).unwrap_or_default();
    Ok((
        format!("{ws_scheme}://{host}{explicit_port}{path}/api/runtime/ws"),
        secure,
    ))
}

/// The address inside an authority's brackets. A URI authority must keep
/// them and `http` hands them back that way, so a rule that parses the
/// address strips them first. A host that is not bracketed comes back as it
/// stands.
pub(super) fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
}

fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(end) = authority.strip_prefix('[') {
        let close = end.find(']')?;
        let host = &authority[..close + 2];
        let tail = &authority[close + 2..];
        if tail.is_empty() {
            return Some((host, None));
        }
        let port = tail.strip_prefix(':')?;
        return Some((host, Some(parse_port(port)?)));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(parse_port(port)?))),
        None => Some((authority, None)),
    }
}

fn parse_port(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn valid_https_host(host: &str) -> bool {
    if host.starts_with('[') {
        return unbracketed(host).parse::<std::net::Ipv6Addr>().is_ok();
    }
    if host.is_empty() || host.len() > 253 || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

fn validate_path(path: &str) -> Option<String> {
    if !path.starts_with('/')
        || path.contains('\\')
        || path.contains('?')
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return None;
    }
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return None;
            }
            let pair = &path[i + 1..i + 3];
            if pair.eq_ignore_ascii_case("2f")
                || pair.eq_ignore_ascii_case("5c")
                || pair.eq_ignore_ascii_case("2e")
            {
                return None;
            }
            let decoded = u8::from_str_radix(pair, 16).ok()?;
            if decoded <= 0x20 || decoded == 0x7f {
                return None;
            }
            i += 3;
        } else {
            i += 1;
        }
    }

    let without_final = path.strip_suffix('/').unwrap_or(path);
    if without_final.is_empty() {
        return Some(String::new());
    }
    let segments: Vec<&str> = without_final[1..].split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return None;
    }
    Some(without_final.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn endpoint_grammar_is_literal_and_bounded() {
        for raw in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "http://[::1]",
            "http://[::1]:8080",
            "https://[fe80::1]",
            "HTTP://127.0.0.1:8080",
            "HtTpS://example.test",
            "http://127.0.0.2",
            "http://localhost:8080",
            "http://LocalHost:8080",
            "https://example.test/base/",
        ] {
            assert!(parse_endpoint(raw).is_ok(), "{raw}");
        }
        for raw in [
            "",
            "http://[::2]",
            "https://[fe80::1%25eth0]",
            "https://user@example.test",
            "https://example.test?x=1",
            "https://example.test#fragment",
            "https://example.test/a//b",
            "https://example.test/a/../b",
            "https://example.test/a/%2e%2e/b",
            "https://example.test/a/%2F/b",
            "https://example.test/a\\b",
            "https://example.test:99999/",
            "http\u{1F600}",
            "https\u{20AC}",
            "http://ééé",
        ] {
            assert!(parse_endpoint(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn valid_raw_path_gets_one_endpoint_suffix() {
        assert_eq!(
            parse_endpoint("https://example.test/base/").unwrap().0,
            "wss://example.test/base/api/runtime/ws"
        );
        assert_eq!(
            parse_endpoint("http://127.0.0.1:8080").unwrap().0,
            "ws://127.0.0.1:8080/api/runtime/ws"
        );
    }

    #[test]
    fn token_is_not_trimmed_and_enforces_ascii_delimiters() {
        assert!(valid_token_bytes(OsStr::new("abc")).is_some());
        assert!(valid_token_bytes(OsStr::new(" abc")).is_none());
        assert!(valid_token_bytes(OsStr::new("abc ")).is_none());
        assert!(valid_token_bytes(OsStr::new("a\"b")).is_none());
        assert!(valid_token_bytes(OsStr::new("a\\b")).is_none());
        assert!(valid_token_bytes(OsStr::new("é")).is_none());
        let token_4096 = "a".repeat(4096);
        let token_4097 = "a".repeat(4097);
        assert!(valid_token_bytes(OsStr::new(&token_4096)).is_some());
        assert!(valid_token_bytes(OsStr::new(&token_4097)).is_none());
    }

    #[test]
    fn explicit_endpoint_and_profile_beat_environment() {
        let source = ReadRequestSource {
            explicit_url: Some("http://127.0.0.1:9000".into()),
            env_url: Some(OsString::from("https://example.test")),
            token: Some(OsString::from("token")),
            explicit_profile: Some("explicit".into()),
            env_profile: Some(OsString::from("environment")),
        };
        assert!(matches!(
            select_endpoint(&source).unwrap(),
            SelectedEndpoint::Http { .. }
        ));
        assert!(matches!(
            selected_profile_source(&source),
            ProfileSource::Explicit("explicit")
        ));
    }

    fn source_with(
        explicit_url: Option<&str>,
        env_url: Option<&str>,
        explicit_profile: Option<&str>,
        env_profile: Option<&str>,
    ) -> ReadRequestSource {
        ReadRequestSource {
            explicit_url: explicit_url.map(str::to_string),
            env_url: env_url.map(OsString::from),
            token: Some(OsString::from("token")),
            explicit_profile: explicit_profile.map(str::to_string),
            env_profile: env_profile.map(OsString::from),
        }
    }

    /// An exported-but-empty variable is unset, so it selects the local
    /// transport exactly as an absent one does. An explicitly empty
    /// `--daemon-url` is a flag the user gave a value to by mistake and is
    /// still refused.
    #[test]
    fn an_empty_environment_url_is_unset_but_an_empty_flag_is_refused() {
        for env_url in [None, Some(""), Some("   "), Some("\t\n")] {
            let source = source_with(None, env_url, None, None);
            assert!(
                matches!(select_endpoint(&source).unwrap(), SelectedEndpoint::Local),
                "{env_url:?} must select the local transport"
            );
            assert!(!source.env_url_is_set(), "{env_url:?} must read as unset");
        }
        let source = source_with(Some(""), None, None, None);
        let error = select_endpoint(&source).unwrap_err();
        assert_eq!(error.code(), "invalid_endpoint");
        // A value that is present still has to parse, and must not be trimmed
        // into a different one.
        assert!(select_endpoint(&source_with(None, Some("not a url"), None, None)).is_err());
    }

    /// Both selectors distinguish an empty default from a whitespace-only name.
    #[test]
    fn an_empty_profile_selection_is_the_default_and_a_blank_one_is_a_name() {
        for env_profile in [None, Some("")] {
            let source = source_with(None, None, None, env_profile);
            assert!(
                matches!(selected_profile_source(&source), ProfileSource::Default),
                "{env_profile:?} must select the default profile"
            );
        }
        let source = source_with(None, None, Some("   "), None);
        assert!(
            matches!(
                selected_profile_source(&source),
                ProfileSource::Explicit("   ")
            ),
            "a blank flag is a profile name, which the local path refuses"
        );
        // The local profile resolver tests emptiness without trimming.
        for env_profile in [Some("   "), Some("\t\n")] {
            let source = source_with(None, None, None, env_profile);
            assert!(
                matches!(
                    selected_profile_source(&source),
                    ProfileSource::Environment(value) if value == OsStr::new(env_profile.unwrap())
                ),
                "{env_profile:?} is a profile name, which the local path refuses"
            );
        }
        // Only a genuinely empty variable means "no selection", which is what
        // the loop above the first one covers.
        let source = source_with(None, None, None, Some(""));
        assert!(
            matches!(selected_profile_source(&source), ProfileSource::Default),
            "an empty variable is unset"
        );
        // An explicit `-p ''` also means "no selection", and it still wins
        // over the variable, because that is how the local path reads it.
        let source = source_with(None, None, Some(""), Some("environment"));
        assert!(matches!(
            selected_profile_source(&source),
            ProfileSource::Default
        ));
        let source = source_with(None, None, Some("named"), Some(""));
        assert!(matches!(
            selected_profile_source(&source),
            ProfileSource::Explicit("named")
        ));
    }

    /// Empty environment selection remains captured but selects the default profile.
    #[test]
    fn an_empty_profile_variable_is_captured_and_still_reads_as_the_default() {
        let cli = {
            let _env = crate::session::test_support::EnvGuard::unset(&[PROFILE_ENV]);
            super::super::Cli::parse_from(["aoe", "ps"])
        };
        {
            let _env = crate::session::test_support::EnvGuard::set(&[(PROFILE_ENV, "")]);
            let source = read_request_source(&cli);
            assert_eq!(
                source.env_profile.as_deref(),
                Some(OsStr::new("")),
                "an empty variable must survive capture: `main` derives the write scope from it"
            );
            assert!(
                matches!(selected_profile_source(&source), ProfileSource::Default),
                "an empty variable is unset"
            );
        }
        {
            let _env = crate::session::test_support::EnvGuard::set(&[(PROFILE_ENV, "   ")]);
            let source = read_request_source(&cli);
            assert_eq!(
                source.env_profile.as_deref(),
                Some(OsStr::new("   ")),
                "a blank variable must survive capture too"
            );
            assert!(
                matches!(
                    selected_profile_source(&source),
                    ProfileSource::Environment(value) if value == OsStr::new("   ")
                ),
                "a blank variable is a profile name, and the local path refuses it"
            );
        }
    }
}
