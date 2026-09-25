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

pub(crate) fn read_request_source(cli: &super::Cli) -> ReadRequestSource {
    ReadRequestSource {
        explicit_url: cli.daemon_url.clone(),
        env_url: std::env::var_os(URL_ENV),
        token: std::env::var_os(TOKEN_ENV),
        explicit_profile: cli.profile.clone(),
        env_profile: std::env::var_os(PROFILE_ENV),
    }
}

#[derive(Debug)]
pub(crate) enum SelectedEndpoint {
    Local,
    Http {
        /// Boxed: the local branch carries no data, and a bare 224-byte request
        /// would make every `SelectedEndpoint` that wide.
        request: Box<tokio_tungstenite::tungstenite::handshake::client::Request>,
    },
}

pub(crate) fn select_endpoint(source: &ReadRequestSource) -> Result<SelectedEndpoint, ReadFailure> {
    let selected = match (&source.explicit_url, &source.env_url) {
        (Some(_), _) => source.explicit_url.as_deref(),
        (None, Some(value)) => Some(
            value
                .to_str()
                .ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?,
        ),
        (None, None) => return Ok(SelectedEndpoint::Local),
    };
    let raw = selected.ok_or_else(|| ReadFailure::pre("invalid_endpoint"))?;
    if raw.is_empty() {
        return Err(ReadFailure::pre("invalid_endpoint"));
    }

    let (request_url, secure) = parse_endpoint(raw)?;
    let token = source
        .token
        .as_deref()
        .ok_or_else(|| ReadFailure::pre("invalid_token"))?;
    validate_token(token)?;

    let mut request = request_url
        .into_client_request()
        .map_err(|_| ReadFailure::pre("invalid_endpoint"))?;
    let value = valid_token_bytes(token).ok_or_else(|| ReadFailure::pre("invalid_token"))?;
    let header = format!(
        "Bearer {}",
        String::from_utf8(value.to_vec()).expect("validated ASCII")
    );
    request.headers_mut().insert(
        "Authorization",
        header
            .parse()
            .map_err(|_| ReadFailure::pre("invalid_token"))?,
    );
    debug_assert_eq!(secure, request.uri().scheme_str() == Some("wss"));
    Ok(SelectedEndpoint::Http {
        request: Box::new(request),
    })
}

pub(crate) fn selected_profile_source(source: &ReadRequestSource) -> ProfileSource<'_> {
    if let Some(value) = source.explicit_profile.as_deref() {
        return ProfileSource::Explicit(value);
    }
    if let Some(value) = source.env_profile.as_deref() {
        return ProfileSource::Environment(value);
    }
    ProfileSource::Default
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProfileSource<'a> {
    Explicit(&'a str),
    Environment(&'a OsStr),
    Default,
}

fn validate_token(token: &OsStr) -> Result<(), ReadFailure> {
    valid_token_bytes(token)
        .is_some()
        .then_some(())
        .ok_or_else(|| ReadFailure::pre("invalid_token"))
}

fn valid_token_bytes(token: &OsStr) -> Option<&[u8]> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        token.as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = token.to_str().map(str::as_bytes)?;

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

    let (rest, secure) = if raw.len() >= 7 && raw[..7].eq_ignore_ascii_case("http://") {
        (&raw[7..], false)
    } else if raw.len() >= 8 && raw[..8].eq_ignore_ascii_case("https://") {
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
    if !secure && host != "127.0.0.1" && host != "[::1]" {
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
    if let Some(inner) = host.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
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

    #[test]
    fn endpoint_grammar_is_literal_and_bounded() {
        for raw in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "http://[::1]",
            "HTTP://127.0.0.1:8080",
            "HtTpS://example.test",
            "https://example.test/base/",
        ] {
            assert!(parse_endpoint(raw).is_ok(), "{raw}");
        }
        for raw in [
            "",
            "http://localhost:8080",
            "http://127.0.0.2",
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
}
