//! The exposed daemon's passphrase: generated once, remembered for this
//! process, and persisted owner-only so reopening the dialog shows the same
//! words rather than a placeholder.

use std::sync::Mutex;

use rand::prelude::IndexedRandom;

use super::words::PASSPHRASE_WORDS;

/// Passphrase of the tunnel this TUI process started, so reopening the
/// dialog re-displays it instead of the "set at startup" placeholder.
pub(super) static LAST_SPAWNED_PASSPHRASE: Mutex<Option<String>> = Mutex::new(None);

pub(super) fn remember_passphrase(pp: &str) {
    if let Ok(mut guard) = LAST_SPAWNED_PASSPHRASE.lock() {
        *guard = Some(pp.to_string());
    }
}

pub(super) fn recall_passphrase_in_memory() -> Option<String> {
    LAST_SPAWNED_PASSPHRASE.lock().ok()?.clone()
}

pub(super) fn recall_passphrase() -> Option<String> {
    if let Some(pp) = recall_passphrase_in_memory() {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from in-memory cache");
        return Some(pp);
    }
    // Durable saved passphrase (survives stop/start cycles).
    if let Some(pp) = load_saved_passphrase() {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.saved_passphrase");
        return Some(pp);
    }
    // Ephemeral file written by the server on startup. Lets the TUI
    // display the passphrase when the daemon was launched from the CLI.
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.passphrase")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.passphrase on disk");
        Some(trimmed.to_string())
    }
}

/// Load the durable saved passphrase that persists across daemon
/// stop/start cycles. Returns None if no saved passphrase exists.
pub(super) fn load_saved_passphrase() -> Option<String> {
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.saved_passphrase")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Persist a passphrase to the durable file that survives daemon
/// stop/start cycles. Written with owner-only permissions.
pub(super) fn save_passphrase_to_disk(pp: &str) {
    if let Ok(dir) = crate::session::get_app_dir() {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(dir.join("serve.saved_passphrase"))
            {
                let _ = file.write_all(pp.as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(dir.join("serve.saved_passphrase"), pp);
        }
    }
}

/// Load the saved passphrase if one exists, otherwise generate a
/// fresh random one and save it for future launches.
pub(super) fn load_or_generate_passphrase() -> String {
    if let Some(pp) = load_saved_passphrase() {
        return pp;
    }
    let pp = generate_passphrase();
    save_passphrase_to_disk(&pp);
    pp
}

/// Generate a four-word lowercase passphrase (1Password / diceware style).
/// Four words from a ~500-word list gives ~35 bits of entropy, which as a
/// *second* factor on top of the URL token is plenty; far easier to type
/// on a phone keyboard than a random alphanumeric soup.
pub(super) fn generate_passphrase() -> String {
    let mut rng = rand::rng();
    let words: Vec<&'static str> = (0..4)
        .map(|_| {
            *PASSPHRASE_WORDS
                .choose(&mut rng)
                .expect("wordlist nonempty")
        })
        .collect();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_is_four_lowercase_words() {
        let pw = generate_passphrase();
        let words: Vec<&str> = pw.split(' ').collect();
        assert_eq!(words.len(), 4, "passphrase should be 4 words: {:?}", pw);
        for w in &words {
            assert!(!w.is_empty(), "empty word in passphrase: {:?}", pw);
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "non-lowercase-letter in word {:?} of {:?}",
                w,
                pw
            );
        }
    }

    #[test]
    fn passphrase_words_are_from_the_wordlist() {
        let pw = generate_passphrase();
        for w in pw.split(' ') {
            assert!(
                PASSPHRASE_WORDS.contains(&w),
                "word {:?} not in the embedded wordlist",
                w
            );
        }
    }

    #[test]
    fn wordlist_is_well_formed() {
        assert!(
            PASSPHRASE_WORDS.len() >= 256,
            "wordlist too small for reasonable entropy: {}",
            PASSPHRASE_WORDS.len()
        );
        for w in PASSPHRASE_WORDS {
            assert!(!w.is_empty(), "empty word in list");
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "non-lowercase word in list: {:?}",
                w
            );
        }
    }

    // The only test touching the module-global LAST_SPAWNED_PASSPHRASE;
    // the in-memory helpers keep it off the real serve.passphrase file.
    #[test]
    fn passphrase_cache_roundtrip() {
        for passphrase in ["four word diceware phrase", "a different phrase later"] {
            remember_passphrase(passphrase);
            assert_eq!(recall_passphrase_in_memory().as_deref(), Some(passphrase));
        }
    }
}
