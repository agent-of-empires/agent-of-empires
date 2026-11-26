//! Log the sandboxes in: the sign-in link, then the code the login client shows after
//! signing in. The code is masked, zeroed once used and never logged.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::sandbox_login::code::{CodeError, LoginCode, MAX_CODE_LEN};
use crate::tui::components::hint_buttons::HintButtons;
use crate::tui::sandbox_login_poller::StartFailure;
use crate::tui::styles::Theme;

const MASK: char = '•';
const WIDTH: u16 = 76;

/// What the dialog asks its owner to do; the dialog stays open for all but a cancel.
#[derive(Debug)]
pub(crate) enum LoginRequest {
    Code(LoginCode),
    StartAgain { force: bool },
    OpenLink,
    CopyLink,
}

enum Phase {
    Starting,
    Ready {
        url: String,
        submitting: bool,
    },
    /// The login is over (rejected code, timeout): only a new start gives a new link.
    Ended {
        message: String,
    },
    StartFailed {
        message: String,
        failure: StartFailure,
    },
}

/// The typed code in a buffer that never moves: typing cannot leave copies behind in a
/// reallocated `Vec`, and `wipe` zeroes every cell. The `&str` of a paste and the
/// `String` the terminal event carried are copies this dialog cannot wipe.
struct CodeBuffer {
    chars: [char; MAX_CODE_LEN],
    len: usize,
}

impl CodeBuffer {
    fn new() -> Self {
        Self {
            chars: ['\0'; MAX_CODE_LEN],
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// False when the buffer is full and `c` was dropped.
    fn push(&mut self, c: char) -> bool {
        let Some(cell) = self.chars.get_mut(self.len) else {
            return false;
        };
        *cell = c;
        self.len += 1;
        true
    }

    fn pop(&mut self) {
        if self.len > 0 {
            self.len -= 1;
            self.chars[self.len] = '\0';
        }
    }

    fn wipe(&mut self) {
        self.chars.fill('\0');
        std::hint::black_box(&self.chars);
        self.len = 0;
    }

    /// The code as a `String` sized up front, so building it never reallocates.
    fn to_string_exact(&self) -> String {
        let mut text = String::with_capacity(self.len * 4);
        text.extend(&self.chars[..self.len]);
        text
    }

    #[cfg(test)]
    fn is_zeroed(&self) -> bool {
        self.len == 0 && self.chars.iter().all(|c| *c == '\0')
    }
}

pub(crate) struct SandboxLoginDialog {
    phase: Phase,
    code: CodeBuffer,
    notice: Option<String>,
    force: bool,
    /// The `aoe sandbox login` line that logs in the same file, for a login that fails here.
    recovery: Option<String>,
    footer: HintButtons,
}

impl SandboxLoginDialog {
    pub fn new() -> Self {
        Self {
            phase: Phase::Starting,
            code: CodeBuffer::new(),
            notice: None,
            force: false,
            recovery: None,
            footer: HintButtons::default(),
        }
    }

    /// Shows `recovery`, the command line of the login this dialog runs, once the login is
    /// ready or has failed.
    pub fn with_recovery(mut self, recovery: String) -> Self {
        self.recovery = Some(recovery).filter(|r| !r.is_empty());
        self
    }

    pub fn url(&self) -> Option<&str> {
        match &self.phase {
            Phase::Ready { url, .. } => Some(url),
            _ => None,
        }
    }

    pub fn set_ready(&mut self, url: String) {
        self.wipe_code();
        self.notice = None;
        self.phase = Phase::Ready {
            url,
            submitting: false,
        };
    }

    pub fn set_ended(&mut self, message: String) {
        self.wipe_code();
        self.phase = Phase::Ended { message };
    }

    pub fn set_start_failed(&mut self, message: String, failure: StartFailure) {
        self.wipe_code();
        self.phase = Phase::StartFailed { message, failure };
    }

    fn wipe_code(&mut self) {
        self.code.wipe();
    }

    /// Still waiting for the login thread: no final event has arrived.
    pub fn awaiting_worker(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Ready { .. })
    }

    fn typing(&self) -> bool {
        matches!(
            self.phase,
            Phase::Ready {
                submitting: false,
                ..
            }
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<LoginRequest> {
        match &self.phase {
            Phase::Starting => match key.code {
                KeyCode::Esc => DialogResult::Cancel,
                _ => DialogResult::Continue,
            },
            // Cancelling mid token exchange would end a login that may be about to succeed.
            Phase::Ready {
                submitting: true, ..
            } => DialogResult::Continue,
            Phase::Ready { .. } => self.handle_ready_key(key),
            Phase::StartFailed {
                failure: StartFailure::NotSeparateLogin | StartFailure::Unsupported,
                ..
            } => match key.code {
                KeyCode::Esc => DialogResult::Cancel,
                _ => DialogResult::Continue,
            },
            Phase::Ended { .. } | Phase::StartFailed { .. } => match key.code {
                KeyCode::Esc => DialogResult::Cancel,
                KeyCode::Enter => self.start_again(),
                _ => DialogResult::Continue,
            },
        }
    }

    fn handle_ready_key(&mut self, key: KeyEvent) -> DialogResult<LoginRequest> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.wipe_code();
                DialogResult::Cancel
            }
            KeyCode::Enter => self.submit_code(),
            KeyCode::Char('o') if ctrl => DialogResult::Submit(LoginRequest::OpenLink),
            KeyCode::Char('y') if ctrl => DialogResult::Submit(LoginRequest::CopyLink),
            KeyCode::Backspace => {
                self.code.pop();
                self.notice = None;
                DialogResult::Continue
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.notice = (!self.code.push(c)).then(|| CodeError::TooLong.to_string());
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn submit_code(&mut self) -> DialogResult<LoginRequest> {
        // `parse` wipes the string it is given when the code is not valid.
        match LoginCode::parse(self.code.to_string_exact()) {
            Ok(code) => {
                self.wipe_code();
                self.notice = None;
                if let Phase::Ready { submitting, .. } = &mut self.phase {
                    *submitting = true;
                }
                DialogResult::Submit(LoginRequest::Code(code))
            }
            Err(invalid) => {
                self.notice = Some(invalid.to_string());
                DialogResult::Continue
            }
        }
    }

    fn start_again(&mut self) -> DialogResult<LoginRequest> {
        if matches!(
            self.phase,
            Phase::StartFailed {
                failure: StartFailure::AlreadyOk,
                ..
            }
        ) {
            self.force = true;
        }
        self.wipe_code();
        self.notice = None;
        self.phase = Phase::Starting;
        DialogResult::Submit(LoginRequest::StartAgain { force: self.force })
    }

    pub fn handle_paste(&mut self, text: &str) {
        if !self.typing() {
            return;
        }
        self.notice = None;
        for c in text.chars().filter(|c| !c.is_control()) {
            if !self.code.push(c) {
                // Stop at the first dropped char: a huge paste is not walked to its end.
                self.notice = Some(CodeError::TooLong.to_string());
                break;
            }
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<KeyEvent> {
        self.footer.key_at(col, row)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.footer.handle_hover(col, row)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = WIDTH.min(area.width);
        let text_w = usize::from(width.saturating_sub(6)).max(1);
        let rows = |text: &str| text.chars().count().div_ceil(text_w).max(1) as u16;
        // Prose wraps at word ends and can need a row more than its length suggests.
        let prose = |text: &str| rows(text) + 1;
        let dim = Style::default().fg(theme.dimmed);
        let plain = Style::default().fg(theme.text);
        let error = Style::default().fg(theme.error);

        // (text, style, height) per block; the field is drawn from `self.code`.
        let mut blocks: Vec<(Option<Paragraph>, u16)> = Vec::new();
        let mut hints: Vec<(&str, &str, KeyCode)> = Vec::new();
        match &self.phase {
            Phase::Starting => {
                blocks.push((Some(Paragraph::new("Starting the login…").style(plain)), 1));
                hints.push(("Esc", "cancel", KeyCode::Esc));
            }
            Phase::Ready { url, submitting } => {
                let intro = "Sign in in your browser, then paste the code it shows. \
                             The login applies to every sandbox of this agent.";
                blocks.push((
                    Some(Paragraph::new(intro).style(plain).wrap(Wrap { trim: true })),
                    prose(intro),
                ));
                blocks.push((None, 1));
                blocks.push((
                    Some(
                        Paragraph::new(url.as_str())
                            .style(Style::default().fg(theme.accent))
                            .wrap(Wrap { trim: false }),
                    ),
                    rows(url),
                ));
                blocks.push((None, 1));
                if *submitting {
                    blocks.push((Some(Paragraph::new("Checking the code…").style(dim)), 1));
                } else {
                    let mut spans = vec![
                        Span::styled("Sign-in code:", Style::default().fg(theme.accent)),
                        Span::raw(" "),
                        Span::styled(
                            MASK.to_string().repeat(self.code.len()),
                            Style::default().fg(theme.accent),
                        ),
                    ];
                    spans.push(Span::styled(
                        " ",
                        Style::default().fg(theme.background).bg(theme.accent),
                    ));
                    blocks.push((Some(Paragraph::new(Line::from(spans))), 1));
                    hints.push(("Enter", "submit", KeyCode::Enter));
                    hints.push(("Ctrl+O", "open link", KeyCode::Null));
                    hints.push(("Ctrl+Y", "copy link", KeyCode::Null));
                    hints.push(("Esc", "cancel", KeyCode::Esc));
                }
                if let Some(notice) = &self.notice {
                    blocks.push((
                        Some(
                            Paragraph::new(notice.as_str())
                                .style(error)
                                .wrap(Wrap { trim: true }),
                        ),
                        prose(notice),
                    ));
                }
            }
            Phase::Ended { message } => {
                blocks.push((
                    Some(
                        Paragraph::new(message.as_str())
                            .style(error)
                            .wrap(Wrap { trim: true }),
                    ),
                    prose(message),
                ));
                hints.push(("Enter", "start again", KeyCode::Enter));
                hints.push(("Esc", "close", KeyCode::Esc));
            }
            Phase::StartFailed { message, failure } => {
                blocks.push((
                    Some(
                        Paragraph::new(message.as_str())
                            .style(error)
                            .wrap(Wrap { trim: true }),
                    ),
                    prose(message),
                ));
                match failure {
                    StartFailure::AlreadyOk => {
                        hints.push(("Enter", "log in again", KeyCode::Enter))
                    }
                    StartFailure::Retry => hints.push(("Enter", "try again", KeyCode::Enter)),
                    // Starting again cannot help while the setting is off or the runtime
                    // cannot run the helper.
                    StartFailure::NotSeparateLogin | StartFailure::Unsupported => {}
                }
                hints.push(("Esc", "close", KeyCode::Esc));
            }
        }

        // The same login as a command line, for when this dialog cannot finish it.
        if let Some(recovery) = self
            .recovery
            .as_deref()
            .filter(|_| !matches!(self.phase, Phase::Starting))
        {
            blocks.push((None, 1));
            let line = format!("Or run: {recovery}");
            let height = rows(&line);
            blocks.push((
                Some(Paragraph::new(line).style(dim).wrap(Wrap { trim: false })),
                height,
            ));
        }

        let body_height: u16 = blocks.iter().map(|(_, h)| h).sum();
        // Borders, margin, a spacer and the hint row.
        let height = body_height + 6;
        let block = super::dialog_block(" Sandbox login ", theme).padding(Padding::horizontal(1));
        let (_, inner) = super::render_dialog_frame(frame, area, width, height, block);

        let mut constraints: Vec<Constraint> =
            blocks.iter().map(|(_, h)| Constraint::Length(*h)).collect();
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Min(1));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);
        for (i, (paragraph, _)) in blocks.into_iter().enumerate() {
            if let Some(paragraph) = paragraph {
                frame.render_widget(paragraph, chunks[i]);
            }
        }
        let footer = chunks[chunks.len() - 1];
        if hints.is_empty() {
            self.footer.clear();
        } else {
            self.footer
                .render(frame, footer, theme, &hints, Alignment::Left);
        }
    }
}

impl Default for SandboxLoginDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SandboxLoginDialog {
    fn drop(&mut self) {
        self.wipe_code();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_render::draw;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(dialog: &mut SandboxLoginDialog, text: &str) {
        for c in text.chars() {
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Char(c))),
                DialogResult::Continue
            ));
        }
    }

    fn ready(url: &str) -> SandboxLoginDialog {
        let mut dialog = SandboxLoginDialog::new();
        dialog.set_ready(url.to_string());
        dialog
    }

    fn screen(dialog: &mut SandboxLoginDialog) -> String {
        let buf = draw(100, 30, |f, theme| dialog.render(f, f.area(), theme));
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_code(result: DialogResult<LoginRequest>, want: &str) {
        match result {
            DialogResult::Submit(LoginRequest::Code(code)) => assert_eq!(code.as_str(), want),
            other => panic!("expected the code to be submitted, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_login_dialog_enter_submits_the_typed_code() {
        let mut dialog = ready("https://example.test/login");
        type_text(&mut dialog, "abc#def");
        dialog.handle_key(key(KeyCode::Backspace));
        type_text(&mut dialog, "f ");
        assert_code(dialog.handle_key(key(KeyCode::Enter)), "abc#def");
    }

    #[test]
    fn sandbox_login_dialog_a_pasted_code_never_submits_on_its_newline() {
        let mut dialog = ready("https://example.test/login");
        dialog.handle_paste("pasted#code\r\n");
        assert_code(dialog.handle_key(key(KeyCode::Enter)), "pasted#code");
    }

    #[test]
    fn sandbox_login_dialog_an_invalid_code_keeps_the_input_and_says_why() {
        let mut dialog = ready("https://example.test/login");
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert!(screen(&mut dialog).contains("the code is empty"));
        type_text(&mut dialog, "a b");
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        let shown = screen(&mut dialog);
        assert!(shown.contains("printable ASCII without spaces"), "{shown}");
        assert!(shown.contains("•••"), "the input is kept: {shown}");
        // Editing clears the message; the corrected code goes through.
        dialog.handle_key(key(KeyCode::Backspace));
        dialog.handle_key(key(KeyCode::Backspace));
        assert!(!screen(&mut dialog).contains("printable ASCII"));
        type_text(&mut dialog, "b");
        assert_code(dialog.handle_key(key(KeyCode::Enter)), "ab");
    }

    #[test]
    fn sandbox_login_dialog_masks_the_code_and_zeroes_it_after_submit() {
        let mut dialog = ready("https://example.test/login");
        type_text(&mut dialog, "sEcret#code");
        let shown = screen(&mut dialog);
        assert!(shown.contains(&MASK.to_string().repeat(11)), "{shown}");
        assert!(!shown.contains("sEcret"), "{shown}");
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(LoginRequest::Code(_))
        ));
        assert!(dialog.code.is_zeroed());
        assert!(!screen(&mut dialog).contains(MASK));
    }

    #[test]
    fn sandbox_login_dialog_cannot_be_cancelled_while_the_code_is_in_flight() {
        let mut dialog = ready("https://example.test/login");
        type_text(&mut dialog, "abc");
        dialog.handle_key(key(KeyCode::Enter));
        for k in [key(KeyCode::Esc), key(KeyCode::Enter), ctrl('o')] {
            assert!(matches!(dialog.handle_key(k), DialogResult::Continue));
        }
        type_text(&mut dialog, "zzz");
        dialog.handle_paste("zzz");
        assert!(dialog.code.is_zeroed());
        assert!(screen(&mut dialog).contains("Checking the code"));
    }

    #[test]
    fn sandbox_login_dialog_esc_cancels_while_starting_and_while_waiting_for_the_code() {
        let mut starting = SandboxLoginDialog::new();
        assert!(matches!(
            starting.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        let mut waiting = ready("https://example.test/login");
        type_text(&mut waiting, "abc");
        assert!(matches!(
            waiting.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        assert!(waiting.code.is_zeroed());
    }

    /// The command line of the same login stays visible whenever the dialog is more than
    /// "starting": next to the link to sign in with, and with the reason a login ended or
    /// could not start.
    #[test]
    fn sandbox_login_dialog_shows_the_recovery_command_once_past_starting() {
        let recovery = "aoe sandbox login -p work --tool personal --command claude";
        let make = || SandboxLoginDialog::new().with_recovery(recovery.to_string());
        let mut starting = make();
        assert!(!screen(&mut starting).contains("aoe sandbox login"));
        let mut ready = make();
        ready.set_ready("https://example.test/login".to_string());
        assert!(screen(&mut ready).contains(recovery));
        let mut ended = make();
        ended.set_ended("The login ended.".to_string());
        assert!(screen(&mut ended).contains(recovery));
        let mut failed = make();
        failed.set_start_failed("boom".to_string(), StartFailure::Retry);
        assert!(screen(&mut failed).contains(recovery));
        let mut plain = SandboxLoginDialog::new();
        plain.set_ready("https://example.test/login".to_string());
        assert!(!screen(&mut plain).contains("aoe sandbox login"));
    }

    #[test]
    fn sandbox_login_dialog_shows_the_whole_link_and_can_open_or_copy_it() {
        let url = format!("https://example.test/oauth?state={}", "x".repeat(300));
        let mut dialog = ready(&url);
        let shown: String = screen(&mut dialog)
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        assert!(shown.contains(&url), "{shown}");
        assert_eq!(dialog.url(), Some(url.as_str()));
        assert!(matches!(
            dialog.handle_key(ctrl('o')),
            DialogResult::Submit(LoginRequest::OpenLink)
        ));
        assert!(matches!(
            dialog.handle_key(ctrl('y')),
            DialogResult::Submit(LoginRequest::CopyLink)
        ));
        // Control chords are not part of the code.
        assert_code(
            {
                type_text(&mut dialog, "k");
                dialog.handle_key(key(KeyCode::Enter))
            },
            "k",
        );
    }

    #[test]
    fn sandbox_login_dialog_an_ended_login_offers_a_new_link() {
        let mut dialog = ready("https://example.test/login");
        type_text(&mut dialog, "abc");
        dialog.set_ended("That login ended. Start again for a new link.".to_string());
        assert!(dialog.code.is_zeroed());
        assert_eq!(dialog.url(), None);
        let shown = screen(&mut dialog);
        assert!(shown.contains("That login ended. Start again for a new link."));
        assert!(shown.contains("start again"));
        assert!(!shown.contains("https://example.test/login"));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(LoginRequest::StartAgain { force: false })
        ));
        assert!(screen(&mut dialog).contains("Starting"));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn sandbox_login_dialog_an_ok_credential_offers_log_in_again_with_force() {
        let mut dialog = SandboxLoginDialog::new();
        dialog.set_start_failed(
            "The sandbox login is already ok.".to_string(),
            StartFailure::AlreadyOk,
        );
        let shown = screen(&mut dialog);
        assert!(shown.contains("log in again"), "{shown}");
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(LoginRequest::StartAgain { force: true })
        ));
        // A later "start again" keeps replacing the credential.
        dialog.set_ended("x".to_string());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(LoginRequest::StartAgain { force: true })
        ));

        let mut other = SandboxLoginDialog::new();
        other.set_start_failed("busy".to_string(), StartFailure::Retry);
        assert!(screen(&mut other).contains("try again"));
        assert!(matches!(
            other.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(LoginRequest::StartAgain { force: false })
        ));
    }

    #[test]
    fn sandbox_login_dialog_caps_the_code_at_the_login_code_limit() {
        use crate::session::sandbox_login::code::CodeError;
        let too_long = CodeError::TooLong.to_string();

        let mut dialog = ready("https://example.test/login");
        dialog.handle_paste(&"a".repeat(8 * 1024 * 1024));
        assert_eq!(dialog.code.len(), MAX_CODE_LEN);
        let shown = screen(&mut dialog);
        assert!(shown.contains(&too_long), "{shown}");
        // Typing past the cap keeps the cap and keeps saying why.
        type_text(&mut dialog, "b");
        assert_eq!(dialog.code.len(), MAX_CODE_LEN);
        assert!(screen(&mut dialog).contains(&too_long));
        // What fits is still a valid code, and room made by Backspace is usable again.
        dialog.handle_key(key(KeyCode::Backspace));
        assert!(!screen(&mut dialog).contains(&too_long));
        type_text(&mut dialog, "b");
        match dialog.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(LoginRequest::Code(code)) => {
                assert_eq!(code.as_bytes().len(), MAX_CODE_LEN);
                assert!(code.as_str().ends_with('b'));
            }
            other => panic!("expected the code, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_login_dialog_every_way_out_zeroes_the_whole_buffer() {
        type Leave = fn(&mut SandboxLoginDialog);
        let ways: [(&str, Leave); 6] = [
            ("submit", |d| {
                d.handle_key(key(KeyCode::Enter));
            }),
            ("cancel", |d| {
                d.handle_key(key(KeyCode::Esc));
            }),
            ("ended", |d| d.set_ended("x".to_string())),
            ("start failed", |d| {
                d.set_start_failed("x".to_string(), StartFailure::Retry)
            }),
            ("new link", |d| {
                d.set_ready("https://example.test/b".to_string())
            }),
            ("backspace", |d| {
                for _ in 0..16 {
                    d.handle_key(key(KeyCode::Backspace));
                }
            }),
        ];
        for (name, leave) in ways {
            let mut dialog = ready("https://example.test/login");
            type_text(&mut dialog, "sEcret#code");
            assert!(!dialog.code.is_zeroed());
            leave(&mut dialog);
            assert!(dialog.code.is_zeroed(), "{name} left the code behind");
        }
    }

    #[test]
    fn sandbox_login_dialog_offers_no_retry_when_the_setting_is_off() {
        let mut dialog = SandboxLoginDialog::new();
        dialog.set_start_failed(
            "needs separate_login".to_string(),
            StartFailure::NotSeparateLogin,
        );
        let shown = screen(&mut dialog);
        assert!(shown.contains("needs separate_login") && shown.contains("close"));
        assert!(!shown.contains("try again") && !shown.contains("start again"));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn sandbox_login_dialog_knows_when_it_waits_for_its_worker() {
        let mut dialog = SandboxLoginDialog::new();
        assert!(dialog.awaiting_worker());
        dialog.set_ready("https://example.test/login".to_string());
        assert!(dialog.awaiting_worker());
        dialog.set_ended("x".to_string());
        assert!(!dialog.awaiting_worker());
        dialog.set_start_failed("x".to_string(), StartFailure::Retry);
        assert!(!dialog.awaiting_worker());
    }

    #[test]
    fn sandbox_login_dialog_footer_hints_are_clickable() {
        let mut dialog = SandboxLoginDialog::new();
        let buf = draw(100, 30, |f, theme| dialog.render(f, f.area(), theme));
        let (x, y) = crate::tui::dialogs::test_render::find(&buf, "Esc cancel");
        assert!(dialog.handle_hover(x, y));
        assert_eq!(dialog.handle_click(x, y), Some(key(KeyCode::Esc)));
    }
}
