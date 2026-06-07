//! Per-binary install hint catalog for OMP ACP.

/// Returns the install command for a known ACP binary, or `None` for
/// unknown commands so callers can fall through to a generic message.
pub fn install_hint_for(binary: &str) -> Option<&'static str> {
    Some(match binary {
        "omp" => {
            "curl -fsSL https://raw.githubusercontent.com/nicepkg/oh-my-pi/main/scripts/install.sh | bash"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_default_registry_binary() {
        for binary in ["omp"] {
            assert!(
                install_hint_for(binary).is_some(),
                "missing install hint for {binary}"
            );
        }
    }

    #[test]
    fn returns_none_for_unknown_binary() {
        assert!(install_hint_for("nonexistent-acp").is_none());
        assert!(install_hint_for("").is_none());
    }
}
