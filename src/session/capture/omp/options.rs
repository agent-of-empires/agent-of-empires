//! Store-affecting OMP CLI flags and shell-syntax screening of `extra_args`.

use super::*;

/// Store-affecting OMP flags extracted from AoE's extra argument string.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OmpCliCaptureOptions {
    pub profile: Option<String>,
    pub session_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl OmpCliCaptureOptions {
    /// Parses shell words, refusing constructs that could obscure store-affecting
    /// argv. Store flags are last-wins like OMP.
    pub(crate) fn parse(extra_args: &str) -> Result<Self> {
        const STORE_FLAGS: [(&str, &str); 3] = [
            ("--cwd", "a directory"),
            ("--profile", "a profile name"),
            ("--session-dir", "a directory"),
        ];
        let shell_words = inspect_shell_syntax(extra_args)?;
        let argv = shell_words::split(extra_args).context("Invalid OMP extra_args quoting")?;
        anyhow::ensure!(
            shell_words.len() == argv.len(),
            "Invalid OMP extra_args tokenization"
        );
        let mut parsed = Self::default();
        let mut index = 0;
        while index < argv.len() {
            let arg = &argv[index];
            if shell_words[index].unquoted_glob && !expansion_cannot_produce_flag(arg) {
                anyhow::bail!("OMP extra_args contains an ambiguous shell expansion");
            }
            if arg == "--" {
                break;
            }
            if arg == "--no-session" || arg.starts_with("--no-session=") {
                anyhow::bail!("OMP --no-session disables breadcrumb capture");
            }
            let store_flag = STORE_FLAGS.iter().find_map(|(flag, what)| {
                let inline = arg.strip_prefix(flag)?;
                match inline.strip_prefix('=') {
                    Some(value) => Some((*flag, *what, Some(value))),
                    None => inline.is_empty().then_some((*flag, *what, None)),
                }
            });
            if let Some((flag, what, inline)) = store_flag {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        if shell_words
                            .get(index + 1)
                            .is_some_and(|word| word.unquoted_tilde || word.unquoted_glob)
                        {
                            anyhow::bail!("OMP {flag} contains an opaque shell expansion");
                        }
                        index += 1;
                        argv.get(index)
                            .map(String::as_str)
                            .filter(|value| flag != "--profile" || !value.starts_with('-'))
                            .unwrap_or_default()
                    }
                };
                anyhow::ensure!(!value.is_empty(), "OMP {flag} requires {what}");
                match flag {
                    "--cwd" => parsed.cwd = Some(PathBuf::from(value)),
                    "--profile" => parsed.profile = Some(value.to_string()),
                    _ => parsed.session_dir = Some(PathBuf::from(value)),
                }
                index += 1;
                continue;
            }
            let consumes_next =
                omp_flag_consumes_next(arg, argv.get(index + 1).map(String::as_str));
            if consumes_next
                && shell_words
                    .get(index + 1)
                    .is_some_and(|word| word.unquoted_glob)
                && argv
                    .get(index + 1)
                    .is_some_and(|value| !expansion_cannot_produce_flag(value))
            {
                anyhow::bail!("OMP extra_args contains an ambiguous shell expansion");
            }
            index += if consumes_next { 2 } else { 1 };
        }
        Ok(parsed)
    }
}

/// Rejects `--api-key`, which would persist in the pane's start command and argv.
pub(crate) fn reject_omp_secret_args(extra_args: &str) -> Result<()> {
    // Expansions first: `--api-key$EMPTY` only becomes `--api-key` in the launch shell.
    inspect_shell_syntax(extra_args)?;
    let argv = shell_words::split(extra_args).context("Invalid OMP extra_args quoting")?;
    anyhow::ensure!(
        !argv
            .iter()
            .any(|arg| arg == "--api-key" || arg.starts_with("--api-key=")),
        "OMP --api-key is not allowed in extra_args; configure the provider API key through the environment"
    );
    Ok(())
}

/// Whether a non-store flag takes the next word as its value (OMP 17.2.10 CLI).
/// Deliberately fail-open: store flags are matched before this runs and all
/// start with `-`, which an unknown flag never swallows. New store-affecting
/// flags belong in `OmpCliCaptureOptions::parse`, not here.
pub(super) fn omp_flag_consumes_next(flag: &str, next: Option<&str>) -> bool {
    const STRING_FLAGS: &[&str] = &[
        "--config",
        "--add-dir",
        "--mode",
        "--fork",
        "--provider",
        "--model",
        "--smol",
        "--slow",
        "--prewalk-into",
        "--plan-yolo-into",
        "--max-time",
        "--service-tier",
        "--api-key",
        "--system-prompt",
        "--append-system-prompt",
        "--provider-session-id",
        "--prompt-cache-key",
        "--models",
        "--tools",
        "--thinking",
        "--export",
        "--hook",
        "--extension",
        "-e",
        "--plugin-dir",
        "--skills",
        "--approval-mode",
        "--trusted-extension",
    ];
    const VALUELESS_FLAGS: &[&str] = &[
        "--help",
        "--version",
        "--allow-home",
        "-c",
        "--continue",
        "--from-claude",
        "--from-codex",
        "--no-tools",
        "--no-lsp",
        "--no-pty",
        "--hide-thinking",
        "--advisor",
        "--prewalk",
        "--no-prewalk",
        "--plan-yolo",
        "--print",
        "--print-thoughts",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--no-title",
        "--auto-approve",
        "--yolo",
    ];
    let Some(next) = next else {
        return false;
    };
    if flag == "--plan" || matches!(flag, "--resume" | "-r" | "--session") {
        return !next.starts_with('-') && !next.is_empty();
    }
    if STRING_FLAGS.contains(&flag) {
        return true;
    }
    flag.starts_with("--")
        && !flag.contains('=')
        && !VALUELESS_FLAGS.contains(&flag)
        && !next.starts_with('-')
}

#[derive(Default)]
pub(super) struct ShellWordInspection {
    unquoted_tilde: bool,
    unquoted_glob: bool,
}

pub(super) fn inspect_shell_syntax(input: &str) -> Result<Vec<ShellWordInspection>> {
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;
    let mut word = ShellWordInspection::default();
    let mut words = Vec::new();
    for byte in input.bytes() {
        if escaped {
            escaped = false;
            in_word = true;
            continue;
        }
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
            }
            Some(b'"') => {
                if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = None;
                } else if matches!(byte, b'$' | b'`') {
                    anyhow::bail!("OMP extra_args contains opaque shell syntax");
                }
            }
            Some(_) => anyhow::bail!("OMP extra_args contains opaque shell syntax"),
            None => match byte {
                b' ' | b'\t' => {
                    if in_word {
                        words.push(word);
                        in_word = false;
                        word = ShellWordInspection::default();
                    }
                }
                b'\\' => {
                    escaped = true;
                    in_word = true;
                }
                b'\'' | b'"' => {
                    quote = Some(byte);
                    in_word = true;
                }
                b'~' if !in_word => {
                    word.unquoted_tilde = true;
                    in_word = true;
                }
                b'*' | b'?' | b'[' | b'{' | b'}' => {
                    word.unquoted_glob = true;
                    in_word = true;
                }
                b'$' | b'`' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'#' | b'\n'
                | b'\r' => anyhow::bail!("OMP extra_args contains opaque shell syntax"),
                _ => in_word = true,
            },
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

pub(super) fn expansion_cannot_produce_flag(word: &str) -> bool {
    word.as_bytes()
        .first()
        .is_some_and(|byte| !matches!(byte, b'-' | b'*' | b'?' | b'[' | b'{' | b'}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_benign_and_store_arguments_last_wins() {
        let parsed = OmpCliCaptureOptions::parse(
            "--model sonnet --cwd old --profile old --yolo --session-dir one --profile=new --cwd=../target --session-dir=two",
        )
        .unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("new"));
        assert_eq!(parsed.session_dir.as_deref(), Some(Path::new("two")));
        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("../target")));
        assert_eq!(
            OmpCliCaptureOptions::parse("-- --profile ignored --no-session").unwrap(),
            OmpCliCaptureOptions::default()
        );
        assert_eq!(
            OmpCliCaptureOptions::parse("--system-prompt --profile work")
                .unwrap()
                .profile,
            None
        );
        assert_eq!(
            OmpCliCaptureOptions::parse("--trusted-extension --session-dir /x")
                .unwrap()
                .session_dir,
            None
        );
        for invalid in [
            "--no-session",
            "--profile",
            "--session-dir=",
            "--cwd=",
            "--model x; echo bad",
        ] {
            assert!(OmpCliCaptureOptions::parse(invalid).is_err(), "{invalid}");
        }
        for profile in ["con", "aux.txt", "com0", "lpt9"] {
            assert!(normalize_profile(Some(profile)).is_err(), "{profile}");
        }
        assert_eq!(
            normalize_profile(Some("valid_profile")).unwrap().as_deref(),
            Some("valid_profile")
        );
        assert!(OmpCliCaptureOptions::parse(
            "--add-dir src/* --system-prompt prompts/*.md --cwd=~/project"
        )
        .is_ok());
    }

    #[test]
    fn rejects_only_unquoted_shell_path_expansions() {
        for expansion in [
            "--cwd ~/project",
            "--cwd=project/*",
            "--cwd=project/?",
            "--cwd=project/[ab]",
            "--cwd=project/{one,two}",
        ] {
            assert!(
                OmpCliCaptureOptions::parse(expansion).is_err(),
                "{expansion}"
            );
        }
        assert_eq!(
            OmpCliCaptureOptions::parse("--add-dir ~/shared --cwd=~/project")
                .unwrap()
                .cwd
                .as_deref(),
            Some(Path::new("~/project"))
        );
        for (literal, expected) in [
            (
                "--cwd='~/project/*?[ab]{one,two}'",
                "~/project/*?[ab]{one,two}",
            ),
            (
                r"--cwd=\~/project/\*/\?/\[ab\]/\{one,two\}",
                "~/project/*/?/[ab]/{one,two}",
            ),
        ] {
            assert_eq!(
                OmpCliCaptureOptions::parse(literal).unwrap().cwd.as_deref(),
                Some(Path::new(expected)),
                "{literal}"
            );
        }
    }
}
