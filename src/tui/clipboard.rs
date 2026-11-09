//! Clipboard output through desktop helpers and streamed OSC 52.
//! OSC 52 has no acknowledgement; terminal-specific limits still apply.

use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    process::{Command, Stdio},
};

use base64::{engine::general_purpose::STANDARD, write::EncoderWriter};

#[derive(Debug)]
pub(crate) struct CopyReport {
    pub bytes: u64,
    pub desktop: Option<&'static str>,
    pub osc52_emitted: bool,
}

pub fn copy_to_clipboard(text: &str) -> usize {
    let result = (|| {
        let mut file = tempfile::tempfile()?;
        file.write_all(text.as_bytes())?;
        copy_file_with_timeout(file, std::time::Duration::from_millis(500))
    })();
    match result {
        Ok(report) => usize::try_from(report.bytes).unwrap_or(usize::MAX),
        Err(error) => {
            tracing::warn!(target: "tui.clipboard", %error, "clipboard output failed");
            0
        }
    }
}

pub(crate) fn copy_file(file: File) -> io::Result<CopyReport> {
    copy_file_with_timeout(file, std::time::Duration::from_secs(30))
}

fn copy_file_with_timeout(mut file: File, timeout: std::time::Duration) -> io::Result<CopyReport> {
    let bytes = file.metadata()?.len();
    let desktop = try_subprocess(&mut file, timeout);
    file.rewind()?;
    let osc52 = write_osc52(
        &mut file,
        &mut io::stdout().lock(),
        std::env::var_os("TMUX").is_some(),
    );
    tracing::info!(target: "tui.clipboard", bytes, desktop = ?desktop, osc52_ok = osc52.is_ok(), "clipboard output");
    if let (Err(desktop), Err(osc52)) = (&desktop, &osc52) {
        return Err(io::Error::other(format!("{desktop}; OSC 52: {osc52}")));
    }
    Ok(CopyReport {
        bytes,
        desktop: desktop.ok(),
        osc52_emitted: osc52.is_ok(),
    })
}

fn try_subprocess(file: &mut File, timeout: std::time::Duration) -> Result<&'static str, String> {
    let mut errors = String::new();
    for (command, arguments) in crate::process::clipboard_commands() {
        match run_subprocess(command, arguments, file, timeout) {
            Ok(()) => return Ok(command),
            Err(error) => {
                if !errors.is_empty() {
                    errors.push_str("; ");
                }
                errors.push_str(&format!("{command}: {error}"));
            }
        }
    }
    Err(if errors.is_empty() {
        "no desktop clipboard helper for this platform".into()
    } else {
        errors
    })
}
fn run_subprocess(
    command: &str,
    arguments: &[&str],
    file: &mut File,
    timeout: std::time::Duration,
) -> io::Result<()> {
    file.rewind()?;
    let mut child = Command::new(command)
        .args(arguments)
        .stdin(file.try_clone()?)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match crate::process::wait_with_timeout(&mut child, timeout)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(io::Error::other(format!("exit {status}"))),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("clipboard helper exceeded {timeout:?}"),
        )),
    }
}

fn write_osc52(
    input: &mut (impl Read + Seek),
    output: &mut impl Write,
    in_tmux: bool,
) -> io::Result<()> {
    if in_tmux {
        input.rewind()?;
        sequence(input, output, b"\x1bPtmux;\x1b\x1b]52;c;", b"\x07\x1b\\")?;
    }
    input.rewind()?;
    sequence(input, output, b"\x1b]52;c;", b"\x07")?;
    output.flush()
}
fn sequence(
    input: &mut impl Read,
    output: &mut impl Write,
    prefix: &[u8],
    suffix: &[u8],
) -> io::Result<()> {
    output.write_all(prefix)?;
    let result = (|| {
        let mut encoded = EncoderWriter::new(&mut *output, &STANDARD);
        io::copy(input, &mut encoded)?;
        encoded.finish()?;
        Ok(())
    })();
    let closed = output.write_all(suffix);
    result.and(closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn osc52_preserves_large_utf8_in_plain_and_tmux_sequences() {
        let text = "é界👩‍👩‍👧‍👧".repeat(65_537);
        let encoded = STANDARD.encode(text.as_bytes());
        for in_tmux in [false, true] {
            let mut output = Vec::new();
            write_osc52(&mut io::Cursor::new(text.as_bytes()), &mut output, in_tmux).unwrap();
            let plain = format!("\x1b]52;c;{encoded}\x07");
            let expected = if in_tmux {
                format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\{plain}")
            } else {
                plain
            };
            assert_eq!(output, expected.as_bytes());
        }
    }
}
