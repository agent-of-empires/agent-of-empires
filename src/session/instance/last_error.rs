/// What a Session's last poll or action explained about its failure, and whether that is the
/// latch for a pane tmux can't show. Runtime only, never saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastError {
    message: Option<String>,
    absence: Option<TmuxAbsence>,
    /// Under an absence, the message is one recorded before it rather than AoE's own tmux text.
    kept: bool,
}

/// Why tmux can't show a Session's pane. Only [`LastError::latch_tmux`] records one, so no text a
/// pane prints can pass for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxAbsence {
    /// tmux reports no such session.
    Gone,
    /// The tmux server stayed unreachable past its window.
    Unreachable,
}

impl LastError {
    /// A plain explanation, not a tmux latch.
    pub fn set(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
        self.absence = None;
        self.kept = false;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `message` explains a pane tmux can't show, for `absence`.
    pub(crate) fn latch_tmux(&mut self, absence: TmuxAbsence, message: impl Into<String>) {
        self.message = Some(message.into());
        self.absence = Some(absence);
        self.kept = false;
    }

    /// The same explanation, now reading `message`: a latch, and whose text it is, stay.
    pub(crate) fn rewrite(&mut self, message: impl Into<String>) {
        if self.message.is_some() {
            self.message = Some(message.into());
        }
    }

    /// tmux confirmed the pane gone: the message stays, now as that latch.
    pub(crate) fn confirm_gone(&mut self) {
        if self.message.is_some() {
            self.kept |= self.absence.is_none();
            self.absence = Some(TmuxAbsence::Gone);
        }
    }

    /// What the message says beyond AoE's own tmux text: all of a plain or kept message, what
    /// follows a tmux lead AoE wrote, and nothing of a bare tmux message.
    pub(crate) fn diagnostic(&self) -> Option<&str> {
        let message = self.message.as_deref()?;
        if self.absence.is_none() || self.kept {
            return Some(message);
        }
        use super::status::{TMUX_SERVER_UNREACHABLE_LEAD, TMUX_SESSION_GONE_LEAD};
        [TMUX_SESSION_GONE_LEAD, TMUX_SERVER_UNREACHABLE_LEAD]
            .into_iter()
            .find_map(|lead| message.strip_prefix(lead))
            .map(str::trim_start)
            .filter(|rest| !rest.is_empty())
    }

    pub(crate) fn absence(&self) -> Option<TmuxAbsence> {
        self.absence
    }

    pub fn as_deref(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn is_some(&self) -> bool {
        self.message.is_some()
    }

    pub fn is_none(&self) -> bool {
        self.message.is_none()
    }

    /// Moves this error out, leaving none behind.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}
