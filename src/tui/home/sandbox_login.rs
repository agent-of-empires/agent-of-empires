//! The sandbox login action: the dialog and the thread that runs the login.

use std::sync::mpsc::TryRecvError;

use super::*;
use crate::session::config::container_config::{
    default_login_tool, login_required_for, login_target,
};
use crate::session::config::SharedCredentialMode;
use crate::session::sandbox_login::helper::LoginError;
use crate::tui::dialogs::{LoginRequest, SandboxLoginDialog};
use crate::tui::sandbox_login_poller::{LoginEvent, SandboxLoginPoller};

const WORKER_STOPPED: &str = "The login worker stopped. Start again.";

/// Only `separate_login` mode has a login to run.
fn login_listed(mode: SharedCredentialMode) -> bool {
    mode == SharedCredentialMode::SeparateLogin
}

/// What the action logs in: the target the launch gate resolves for the selected session,
/// or the profile's default login tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct LoginSelection {
    pub profile: String,
    pub tool: String,
    pub command: Option<String>,
    /// The `aoe sandbox login` line that logs in the same file.
    pub recovery: String,
}

impl HomeView {
    /// Whether the palette lists the login action. Reads only the config: the credential
    /// file is behind a blocking lock a sandbox come-up holds, and the UI thread must
    /// not wait for it.
    pub(super) fn sandbox_login_listed(&self) -> bool {
        login_listed(
            resolve_config_or_warn(&self.config_profile())
                .sandbox
                .shared_credential,
        )
    }

    /// What the login runs for: the target the launch gate resolves for the selected session
    /// when its tool has a sandbox login (its profile, tool and own command: a wrapper agent
    /// or a command override has a file of its own), else the profile's default login tool.
    /// `None` when neither picks one.
    pub(super) fn sandbox_login_target(&self) -> Option<LoginSelection> {
        let selected = self
            .selected_session
            .as_deref()
            .and_then(|id| self.get_instance(id))
            .and_then(|session| {
                let recovery = session.sandbox_login_request()?.login_command();
                Some(LoginSelection {
                    profile: session.effective_profile(),
                    tool: session.tool.clone(),
                    command: Some(session.command.clone()).filter(|c| !c.is_empty()),
                    recovery,
                })
            });
        selected.or_else(|| {
            let profile = self.config_profile();
            let tool = default_login_tool(&profile)?;
            let target = login_target(&profile, &tool)?;
            let recovery = login_required_for(&profile, &tool, None, &target).login_command();
            Some(LoginSelection {
                profile,
                tool,
                command: None,
                recovery,
            })
        })
    }

    pub(super) fn open_sandbox_login(&mut self) {
        self.sandbox_login_selection = self.sandbox_login_target();
        let recovery = self
            .sandbox_login_selection
            .as_ref()
            .map(|t| t.recovery.clone());
        self.sandbox_login_dialog =
            Some(SandboxLoginDialog::new().with_recovery(recovery.unwrap_or_default()));
        self.start_sandbox_login(false);
    }

    /// Starts the login the dialog opened with, however the selection moved since.
    fn start_sandbox_login(&mut self, force: bool) {
        if let Some(old) = self.sandbox_login_poller.take() {
            old.cancel();
        }
        self.sandbox_login_poller = Some(match self.sandbox_login_selection.clone() {
            Some(target) => {
                SandboxLoginPoller::spawn(target.profile, target.tool, target.command, force)
            }
            None => SandboxLoginPoller::spawn_with(async {
                Err(LoginError::Other(anyhow::anyhow!(
                    "several agents have a sandbox login: select a session of one of them first"
                )))
            }),
        });
    }

    /// Esc: the helper is cancelled in the background, the dialog goes at once.
    pub(super) fn close_sandbox_login(&mut self) {
        self.sandbox_login_dialog = None;
        self.sandbox_login_selection = None;
        if let Some(poller) = self.sandbox_login_poller.take() {
            poller.cancel();
        }
    }

    pub(super) fn run_sandbox_login_request(&mut self, request: LoginRequest) {
        match request {
            LoginRequest::Code(code) => {
                if let Some(poller) = &self.sandbox_login_poller {
                    poller.submit(code);
                }
            }
            LoginRequest::StartAgain { force } => self.start_sandbox_login(force),
            LoginRequest::OpenLink => {
                if let Some(url) = self.sandbox_login_url() {
                    self.open_sign_in_link(
                        &url,
                        crate::tui::open_url::open_url,
                        crate::tui::clipboard::copy_to_clipboard,
                    );
                }
            }
            LoginRequest::CopyLink => {
                if let Some(url) = self.sandbox_login_url() {
                    crate::tui::clipboard::copy_to_clipboard(&url);
                    self.flash_status("copied the sign-in link");
                }
            }
        }
    }

    pub(super) fn open_sign_in_link(
        &mut self,
        url: &str,
        open: impl FnOnce(&str) -> std::io::Result<()>,
        copy: impl FnOnce(&str) -> usize,
    ) {
        // The link carries the login's `state`: it never goes into the status bar.
        let status = match open(url) {
            Ok(()) => "opened the sign-in link",
            Err(_) => {
                copy(url);
                "no browser here; copied the sign-in link"
            }
        };
        self.flash_status(status);
    }

    fn sandbox_login_url(&self) -> Option<String> {
        self.sandbox_login_dialog
            .as_ref()
            .and_then(|dialog| dialog.url())
            .map(str::to_string)
    }

    pub(super) fn apply_sandbox_login_event(&mut self, event: LoginEvent) {
        match event {
            LoginEvent::LoggedIn => {
                self.close_sandbox_login();
                self.info_dialog = Some(InfoDialog::new(
                    "Logged in",
                    "Sandboxes use the new login from their next start.",
                ));
            }
            event => {
                let Some(dialog) = &mut self.sandbox_login_dialog else {
                    return;
                };
                match event {
                    LoginEvent::Started { url } => dialog.set_ready(url),
                    LoginEvent::Ended { message } => dialog.set_ended(message),
                    LoginEvent::StartFailed { message, failure } => {
                        dialog.set_start_failed(message, failure)
                    }
                    LoginEvent::LoggedIn => {}
                }
            }
        }
    }

    /// Move the login thread's events into the dialog; true when a redraw is needed.
    pub(super) fn tick_sandbox_login(&mut self) -> bool {
        let mut changed = false;
        loop {
            let Some(poller) = &self.sandbox_login_poller else {
                return changed;
            };
            match poller.try_recv() {
                Ok(event) => self.apply_sandbox_login_event(event),
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    // The thread ended: with its final event already shown, nothing more
                    // comes. Without one it died (a panic), and a dialog waiting on it
                    // would swallow every key while a code is in flight.
                    self.sandbox_login_poller = None;
                    if let Some(dialog) = self
                        .sandbox_login_dialog
                        .as_mut()
                        .filter(|d| d.awaiting_worker())
                    {
                        dialog.set_ended(WORKER_STOPPED.to_string());
                    }
                    return true;
                }
            }
            changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_login_is_listed_only_in_separate_login_mode() {
        assert!(!login_listed(SharedCredentialMode::HostSeed));
        assert!(login_listed(SharedCredentialMode::SeparateLogin));
    }
}
