//! A validated, zeroed-on-drop login code.

pub(crate) const MAX_CODE_LEN: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CodeError {
    #[error("the code is empty")]
    Empty,
    #[error("the code is longer than 512 characters")]
    TooLong,
    #[error("the code may only contain printable ASCII without spaces")]
    InvalidCharacter,
}

pub(crate) struct LoginCode(Vec<u8>);

/// Best-effort: `black_box` keeps the zeroing from being optimized away in practice; not a
/// guarantee (copies made elsewhere are not covered). The code is a one-time, PKCE-bound
/// authorization code.
fn wipe(bytes: &mut Vec<u8>) {
    bytes.fill(0);
    std::hint::black_box(&*bytes);
    bytes.clear();
}

impl LoginCode {
    /// Takes ownership so the caller's buffer is the one wiped.
    pub(crate) fn parse(raw: String) -> Result<Self, CodeError> {
        let mut bytes = raw.into_bytes();
        let end = bytes
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(0, |i| i + 1);
        let start = bytes[..end]
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(end);
        let verdict = match &bytes[start..end] {
            [] => Err(CodeError::Empty),
            code if code.len() > MAX_CODE_LEN => Err(CodeError::TooLong),
            code if !code.iter().all(|b| (0x21..=0x7e).contains(b)) => {
                Err(CodeError::InvalidCharacter)
            }
            _ => Ok(()),
        };
        if let Err(error) = verdict {
            wipe(&mut bytes);
            return Err(error);
        }
        let len = end - start;
        bytes.copy_within(start..end, 0);
        bytes[len..].fill(0);
        bytes.truncate(len);
        Ok(Self(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII")
    }
}

impl Drop for LoginCode {
    fn drop(&mut self) {
        wipe(&mut self.0)
    }
}

impl std::fmt::Debug for LoginCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginCode(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_login_code_validation() {
        let ok = LoginCode::parse("  abcDEF123#state-xyz_~.\r\n".to_string()).unwrap();
        assert_eq!(ok.as_str(), "abcDEF123#state-xyz_~.");
        assert_eq!(format!("{ok:?}"), "LoginCode(<redacted>)");
        for (raw, err) in [
            ("", CodeError::Empty),
            (" \t\n", CodeError::Empty),
            ("abc def", CodeError::InvalidCharacter),
            ("abc\x1bdef", CodeError::InvalidCharacter),
            ("abc\u{7f}", CodeError::InvalidCharacter),
            ("abc\u{e9}", CodeError::InvalidCharacter),
        ] {
            assert_eq!(
                LoginCode::parse(raw.to_string()).err(),
                Some(err),
                "{raw:?}"
            );
        }
        assert_eq!(
            LoginCode::parse("a".repeat(MAX_CODE_LEN)).map(|c| c.as_bytes().len()),
            Ok(MAX_CODE_LEN)
        );
        assert_eq!(
            LoginCode::parse("a".repeat(MAX_CODE_LEN + 1)).err(),
            Some(CodeError::TooLong)
        );
    }
}
