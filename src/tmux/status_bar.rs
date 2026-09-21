//! tmux status bar configuration for aoe sessions

use anyhow::Result;
use ratatui::style::Color;

use crate::tui::styles::Theme;

pub struct SandboxDisplay {
    pub container_name: String,
}

fn color_to_tmux(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        _ => "default".to_string(),
    }
}

/// Set the `@aoe_*` user options and paint the themed status bar.
pub fn apply_status_bar(
    session_name: &str,
    title: &str,
    branch: Option<&str>,
    sandbox: Option<&SandboxDisplay>,
    theme: &Theme,
) -> Result<()> {
    // A web attach turns the status line off for the session.
    set_session_option(session_name, "status", "on")?;
    set_session_option(session_name, "@aoe_title", title)?;
    if let Some(branch_name) = branch {
        set_session_option(session_name, "@aoe_branch", branch_name)?;
    }
    if let Some(sandbox_info) = sandbox {
        set_session_option(session_name, "@aoe_sandbox", &sandbox_info.container_name)?;
    }

    let accent = color_to_tmux(theme.accent);
    let fg = color_to_tmux(theme.text);
    let bg = color_to_tmux(theme.background);
    let branch_color = color_to_tmux(theme.branch);
    let sandbox_color = color_to_tmux(theme.sandbox);
    let hint = color_to_tmux(theme.dimmed);

    // "aoe: Title | branch | [container] | 14:30"
    let status_format = format!(
        " #[fg={accent},bold]aoe#[fg={fg},nobold]: \
         #{{@aoe_title}}\
         #{{?#{{@aoe_branch}}, #[fg={branch_color}]| #{{@aoe_branch}}#[fg={fg}],}}\
         #{{?#{{@aoe_sandbox}}, #[fg={sandbox_color}]\u{2b21} #{{@aoe_sandbox}}#[fg={fg}],}}\
          | %H:%M ",
    );

    set_session_option(session_name, "status-right", &status_format)?;
    set_session_option(session_name, "status-right-length", "80")?;
    set_session_option(session_name, "status-style", &format!("bg={bg},fg={fg}"))?;
    let prefix = crate::tmux::utils::tmux_prefix_display();
    set_session_option(
        session_name,
        "status-left",
        &status_left_format(prefix, &accent, &fg, &hint),
    )?;
    // `#S` expands at paint time and a later rename can lengthen it; tmux only
    // trims at this cap, so over-sizing is free.
    set_session_option(session_name, "status-left-length", "200")?;
    Ok(())
}

/// Session name plus the key back to aoe, chosen per client at paint time: a
/// `switch-client` arrival has `client_last_session` (`prefix L`), an attach
/// does not (`prefix d`).
fn status_left_format(prefix: &str, accent: &str, fg: &str, hint: &str) -> String {
    format!(
        " #[fg={accent},bold]#S#[fg={fg},nobold] \u{2502} #[fg={hint}]{prefix} \
         #{{?client_last_session,{switch} back to aoe,{detach} to detach}} ",
        switch = crate::tmux::utils::SWITCH_BACK_KEY,
        detach = crate::tmux::utils::DETACH_KEY,
    )
}

/// Remove a session-scoped override so the global value applies.
fn set_session_option_unset(session_name: &str, option: &str) -> Result<()> {
    let output = crate::tmux::tmux_command()
        .args(["set-option", "-u", "-t", session_name, option])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "tmux set-option -u {} failed: {}",
            option,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Deadline-bounded because renames reach it; option errors are non-critical.
fn set_session_option(session_name: &str, option: &str, value: &str) -> Result<()> {
    let mut command = crate::tmux::tmux_command();
    command.args(["set-option", "-t", session_name, option, value]);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(target: "tmux.status", "Failed to set tmux option {}: {}", option, stderr);
    }
    Ok(())
}

/// Refresh `@aoe_title` after a rename. Not gated on the `StatusBar` setting:
/// `aoe tmux-status` serves it to users painting their own bar.
pub(crate) fn refresh_session_title(session_name: &str, title: &str) {
    let _ = set_session_option(session_name, "@aoe_title", title);
}

pub fn apply_mouse_option(session_name: &str, enabled: bool) -> Result<()> {
    let value = if enabled { "on" } else { "off" };
    set_session_option(session_name, "mouse", value)
}

/// Apply status bar and mouse settings resolved against `profile`'s config.
pub fn apply_all_tmux_options(
    session_name: &str,
    title: &str,
    branch: Option<&str>,
    sandbox: Option<&SandboxDisplay>,
    profile: &str,
) {
    use crate::session::config::{resolve_tmux_setting, TmuxSetting, TmuxSettingAction};
    use crate::tui::styles::load_theme;

    let config = crate::tmux::tmux_option_config(profile);

    if resolve_tmux_setting(TmuxSetting::StatusBar, &config) == TmuxSettingAction::Apply {
        // tmux takes hex colors itself, so palette mode does not apply here.
        let theme = load_theme(&crate::session::config::resolve_theme_name());
        if let Err(e) = apply_status_bar(session_name, title, branch, sandbox, &theme) {
            tracing::debug!(target: "tmux.status", "Failed to apply tmux status bar: {}", e);
        }
    } else {
        // Not ours to paint: clear any session-scoped overrides (an earlier aoe
        // bar, a web attach's `status off`) so the user's global config governs.
        for option in [
            "status",
            "status-left",
            "status-left-length",
            "status-right",
            "status-right-length",
            "status-style",
        ] {
            let _ = set_session_option_unset(session_name, option);
        }
    }

    match resolve_tmux_setting(TmuxSetting::Mouse, &config) {
        action @ (TmuxSettingAction::Apply | TmuxSettingAction::ForceOff) => {
            let enabled = action == TmuxSettingAction::Apply;
            if let Err(e) = apply_mouse_option(session_name, enabled) {
                tracing::debug!(target: "tmux.status", "Failed to apply tmux mouse option: {}", e);
            }
        }
        // A session option outranks the global one, so leaving it to the user
        // means clearing any value aoe set earlier.
        TmuxSettingAction::LeaveToUser => {
            let _ = set_session_option_unset(session_name, "mouse");
        }
    }
}

pub struct SessionInfo {
    pub title: String,
    pub branch: Option<String>,
    pub sandbox: Option<String>,
}

/// Session info for `aoe tmux-status`.
pub fn get_session_info_for_current() -> Option<SessionInfo> {
    let session_name = crate::tmux::get_current_session_name()?;
    let name_without_prefix = session_name.strip_prefix(crate::tmux::SESSION_PREFIX)?;
    // Fallback from `aoe_<title>_<id>`.
    let title = get_session_option(&session_name, "@aoe_title").unwrap_or_else(|| {
        name_without_prefix
            .rsplit_once('_')
            .map_or(name_without_prefix, |(title, _)| title)
            .to_string()
    });
    Some(SessionInfo {
        title,
        branch: get_session_option(&session_name, "@aoe_branch"),
        sandbox: get_session_option(&session_name, "@aoe_sandbox"),
    })
}

/// Plain text like `aoe: Title | branch [container]`.
pub fn get_status_for_current_session() -> Option<String> {
    let info = get_session_info_for_current()?;
    let mut result = format!("aoe: {}", info.title);
    if let Some(b) = &info.branch {
        result.push_str(" | ");
        result.push_str(b);
    }
    if let Some(s) = &info.sandbox {
        result.push_str(&format!(" [{s}]"));
    }
    Some(result)
}

fn get_session_option(session_name: &str, option: &str) -> Option<String> {
    let output = crate::tmux::tmux_command()
        .args(["show-options", "-t", session_name, "-v", option])
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::{builtin_theme_names, load_theme};

    #[test]
    fn test_color_to_tmux() {
        assert_eq!(color_to_tmux(Color::Rgb(15, 23, 42)), "#0f172a");
        assert_eq!(color_to_tmux(Color::Rgb(255, 255, 255)), "#ffffff");
        assert_eq!(color_to_tmux(Color::Rgb(0, 0, 0)), "#000000");
        assert_eq!(color_to_tmux(Color::Red), "default");
    }

    #[test]
    fn status_left_hint_follows_the_clients_attach_path() {
        assert_eq!(
            status_left_format("Ctrl+b", "#111111", "#222222", "#333333"),
            " #[fg=#111111,bold]#S#[fg=#222222,nobold] \u{2502} #[fg=#333333]Ctrl+b \
             #{?client_last_session,L back to aoe,d to detach} "
        );
    }

    #[test]
    fn test_all_themes_produce_valid_status_bar_colors() {
        for theme_name in builtin_theme_names() {
            let theme = load_theme(theme_name);
            let colors = [
                ("background", color_to_tmux(theme.background)),
                ("text", color_to_tmux(theme.text)),
                ("accent", color_to_tmux(theme.accent)),
                ("branch", color_to_tmux(theme.branch)),
                ("sandbox", color_to_tmux(theme.sandbox)),
                ("dimmed", color_to_tmux(theme.dimmed)),
            ];
            for (field, hex) in &colors {
                assert!(
                    hex.starts_with('#'),
                    "{theme_name}: {field} should be hex, got {hex}"
                );
            }
        }
    }
}
