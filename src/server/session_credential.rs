//! Per-session MCP credential: `hex(HMAC-SHA256(secret, session_id))`.
//!
//! The secret is generated once and persists for the life of the app directory, so a minted
//! credential never needs rotating; what revokes it is the session going away, since callers of
//! `verify` additionally require the session to still be live.

use std::path::Path;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::secure_store::{check_path_security, write_private};

type HmacSha256 = Hmac<Sha256>;

const SECRET_LEN: usize = 32;

pub struct McpSessionSecret([u8; SECRET_LEN]);

impl McpSessionSecret {
    /// Load the secret from `path`, or generate and persist a new one if it does not exist yet.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        check_path_security(path)?;
        match std::fs::read(path) {
            Ok(bytes) => {
                let bytes: [u8; SECRET_LEN] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("mcp session secret file is corrupt"))?;
                Ok(Self(bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                use rand::RngExt;
                let mut bytes = [0u8; SECRET_LEN];
                rand::rng().fill(&mut bytes);
                write_private(path, &bytes)?;
                Ok(Self(bytes))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The bearer credential for `session_id`.
    pub fn credential_for(&self, session_id: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.0).expect("HMAC-SHA256 accepts a 32-byte key");
        mac.update(session_id.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Whether `presented` is the credential for `session_id`, compared in constant time.
    pub fn verify(&self, session_id: &str, presented: &str) -> bool {
        super::auth::constant_time_eq(&self.credential_for(session_id), presented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_is_deterministic_and_session_scoped() {
        let secret = McpSessionSecret([0x42; SECRET_LEN]);
        let a = secret.credential_for("session-a");
        assert_eq!(a, secret.credential_for("session-a"));
        assert_ne!(a, secret.credential_for("session-b"));
    }

    #[test]
    fn verify_accepts_only_the_matching_session_and_credential() {
        let secret = McpSessionSecret([0x7; SECRET_LEN]);
        let cred = secret.credential_for("session-a");
        assert!(secret.verify("session-a", &cred));
        assert!(!secret.verify("session-b", &cred), "wrong session id");
        assert!(!secret.verify("session-a", "garbage"), "wrong credential");
    }

    #[test]
    fn load_or_generate_persists_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_session_secret");

        let first = McpSessionSecret::load_or_generate(&path).unwrap();
        assert!(path.exists());
        let second = McpSessionSecret::load_or_generate(&path).unwrap();

        assert_eq!(
            first.credential_for("session-a"),
            second.credential_for("session-a"),
            "reloading must not mint a new secret"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_or_generate_writes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_session_secret");
        McpSessionSecret::load_or_generate(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    fn write_secret_file(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn load_or_generate_rejects_a_corrupt_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_session_secret");
        write_secret_file(&path, b"too short");

        let err = McpSessionSecret::load_or_generate(&path).err().unwrap();
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn load_or_generate_refuses_an_unsafe_existing_secret() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let set_mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        let loose = dir.path().join("loose");
        write_secret_file(&loose, &[7; SECRET_LEN]);
        set_mode(&loose, 0o644);
        assert!(
            McpSessionSecret::load_or_generate(&loose).is_err(),
            "0644 file"
        );

        let target = dir.path().join("target");
        write_secret_file(&target, &[7; SECRET_LEN]);
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(
            McpSessionSecret::load_or_generate(&link).is_err(),
            "symlink"
        );

        let open_parent = dir.path().join("open_parent");
        std::fs::create_dir(&open_parent).unwrap();
        let inside = open_parent.join("mcp_session_secret");
        write_secret_file(&inside, &[7; SECRET_LEN]);
        set_mode(&open_parent, 0o770);
        assert!(
            McpSessionSecret::load_or_generate(&inside).is_err(),
            "group-writable parent"
        );
        let fresh = open_parent.join("fresh");
        assert!(McpSessionSecret::load_or_generate(&fresh).is_err());
        assert!(
            !fresh.exists(),
            "no secret is generated in a group-writable parent"
        );
    }
}
