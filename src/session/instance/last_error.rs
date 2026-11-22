/// What a Session's last poll or action explained about its failure. Runtime only, never saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastError {
    message: Option<String>,
}

impl LastError {
    pub fn set(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub fn clear(&mut self) {
        self.message = None;
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
