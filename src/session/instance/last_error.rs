/// What a Session's last poll or action explained about its failure, and whether that is the
/// latch for a pane tmux can't show. Runtime only, never saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastError {
    message: Option<String>,
    absence: Option<TmuxAbsence>,
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
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `message` explains a pane tmux can't show, for `absence`.
    pub(crate) fn latch_tmux(&mut self, absence: TmuxAbsence, message: impl Into<String>) {
        self.message = Some(message.into());
        self.absence = Some(absence);
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
