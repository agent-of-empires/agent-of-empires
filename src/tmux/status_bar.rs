//! tmux status bar configuration for aoe sessions

use anyhow::Result;
use ratatui::style::Color;

use crate::tui::styles::Theme;

/// Information about a sandboxed session for status bar display.
pub struct SandboxDisplay {
    pub container_name: String,
}

/// Convert a ratatui Color to a tmux-compatible hex color string (e.g. "#0f172a").
fn color_to_tmux(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        _ => "default".to_string(),
    }
}

/// Apply aoe-styled status bar configuration to a tmux session.
///
/// Sets tmux user options (@aoe_title, @aoe_branch, @aoe_sandbox) and configures
/// the status-right to display session information using theme colors.
pub fn apply_status_bar(
    session_name: &str,
    title: &str,
    branch: Option<&str>,
    sandbox: Option<&SandboxDisplay>,
    theme: &Theme,
) -> Result<()> {
    // Re-enable the status line explicitly: a web attach turns it off
    // for that session (the dashboard renders its own chrome), and the
    // TUI/CLI attach experience wants the themed bar + detach hint back.
    set_session_option(session_name, "status", "on")?;

    // Set the session title as a tmux user option
    set_session_option(session_name, "@aoe_title", title)?;

    // Set branch if provided (for worktree sessions)
    if let Some(branch_name) = branch {
        set_session_option(session_name, "@aoe_branch", branch_name)?;
    }

    // Set sandbox info if running in docker container
    if let Some(sandbox_info) = sandbox {
        set_session_option(session_name, "@aoe_sandbox", &sandbox_info.container_name)?;
    }

    let accent = color_to_tmux(theme.accent);
    let fg = color_to_tmux(theme.text);
    let bg = color_to_tmux(theme.background);
    let branch_color = color_to_tmux(theme.branch);
    let sandbox_color = color_to_tmux(theme.sandbox);
    let hint = color_to_tmux(theme.dimmed);

    // Format: "aoe: Title | branch | [container] | 14:30"
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
        &format!(
            " #[fg={accent},bold]#S#[fg={fg},nobold] \u{2502} #[fg={hint}]{prefix} d#[fg={hint}] to detach ",
        ),
    )?;
    set_session_option(session_name, "status-left-length", "50")?;

    Ok(())
}

/// Set a tmux option for a specific session.
/// Remove a session-scoped option override so the global value applies.
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

fn set_session_option(session_name: &str, option: &str, value: &str) -> Result<()> {
    let output = crate::tmux::tmux_command()
        .args(["set-option", "-t", session_name, option, value])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't fail on option errors - status bar is non-critical
        tracing::debug!(target: "tmux.status", "Failed to set tmux option {}: {}", option, stderr);
    }

    Ok(())
}

/// Apply mouse support option to a tmux session.
/// When enabled, scrolling with the mouse wheel enters copy mode.
pub fn apply_mouse_option(session_name: &str, enabled: bool) -> Result<()> {
    let value = if enabled { "on" } else { "off" };
    set_session_option(session_name, "mouse", value)
}

/// Apply all configured tmux options to a session.
/// This is a unified entry point that applies status bar styling and mouse settings.
pub fn apply_all_tmux_options(
    session_name: &str,
    title: &str,
    branch: Option<&str>,
    sandbox: Option<&SandboxDisplay>,
) {
    use crate::session::config::{should_apply_tmux_mouse, should_apply_tmux_status_bar};
    use crate::tui::styles::load_theme;

    if should_apply_tmux_status_bar() {
        // Theme is a global preference; match the TUI's empty-name fallback
        // (`default`) so the status bar can't paint a different theme.
        let theme_name = crate::session::config::resolve_theme_name();
        // Always use truecolor here: tmux receives hex color values (#rrggbb)
        // and manages its own escape-sequence rendering via TERM/terminfo.
        // Palette mode only affects the TUI's direct terminal output.
        let theme = load_theme(&theme_name);

        if let Err(e) = apply_status_bar(session_name, title, branch, sandbox, &theme) {
            tracing::debug!(target: "tmux.status", "Failed to apply tmux status bar: {}", e);
        }
    } else {
        // aoe's bar is disabled (user preference or their own tmux
        // config). A web attach may have set the session-scoped
        // `status off`, and a previously enabled aoe bar leaves its
        // session-scoped visual overrides behind; unset them all so the
        // user's own global config governs again in real terminals.
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

    if let Some(mouse_enabled) = should_apply_tmux_mouse() {
        if let Err(e) = apply_mouse_option(session_name, mouse_enabled) {
            tracing::debug!(target: "tmux.status", "Failed to apply tmux mouse option: {}", e);
        }
    }
}

/// Session info retrieved from tmux user options.
pub struct SessionInfo {
    pub title: String,
    pub branch: Option<String>,
    pub sandbox: Option<String>,
}

/// Get session info for the current tmux session (used by `aoe tmux-status` command).
/// Returns structured session info for use in user's custom tmux status bar.
pub fn get_session_info_for_current() -> Option<SessionInfo> {
    let session_name = crate::tmux::get_current_session_name()?;

    // Check if this is an aoe session
    if !session_name.starts_with(crate::tmux::SESSION_PREFIX) {
        return None;
    }

    // Try to get the aoe title from tmux user option
    let title = get_session_option(&session_name, "@aoe_title").unwrap_or_else(|| {
        // Fallback: extract title from session name
        // Session names are: aoe_<title>_<id>
        let name_without_prefix = session_name
            .strip_prefix(crate::tmux::SESSION_PREFIX)
            .unwrap_or(&session_name);
        if let Some(last_underscore) = name_without_prefix.rfind('_') {
            name_without_prefix[..last_underscore].to_string()
        } else {
            name_without_prefix.to_string()
        }
    });

    let branch = get_session_option(&session_name, "@aoe_branch");
    let sandbox = get_session_option(&session_name, "@aoe_sandbox");

    Some(SessionInfo {
        title,
        branch,
        sandbox,
    })
}

/// Get formatted status string for the current tmux session.
/// Returns a plain text string like `aoe: Title | branch | [container]`
pub fn get_status_for_current_session() -> Option<String> {
    let info = get_session_info_for_current()?;

    let mut result = format!("aoe: {}", info.title);

    if let Some(b) = &info.branch {
        result.push_str(" | ");
        result.push_str(b);
    }

    if let Some(s) = &info.sandbox {
        result.push_str(" [");
        result.push_str(s);
        result.push(']');
    }

    Some(result)
}

/// Get a tmux option value for a session.
fn get_session_option(session_name: &str, option: &str) -> Option<String> {
    let output = crate::tmux::tmux_command()
        .args(["show-options", "-t", session_name, "-v", option])
        .output()
        .ok()?;

    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::{builtin_theme_names, load_theme};

    #[test]
    fn test_get_status_returns_none_for_non_tmux() {
        // When not in tmux, get_current_session_name returns None
        // so get_status_for_current_session should also return None
        // This test just verifies the function doesn't panic
        let _ = get_status_for_current_session();
    }

    #[test]
    fn test_color_to_tmux_rgb() {
        assert_eq!(color_to_tmux(Color::Rgb(15, 23, 42)), "#0f172a");
        assert_eq!(color_to_tmux(Color::Rgb(255, 255, 255)), "#ffffff");
        assert_eq!(color_to_tmux(Color::Rgb(0, 0, 0)), "#000000");
    }

    #[test]
    fn test_color_to_tmux_non_rgb_fallback() {
        assert_eq!(color_to_tmux(Color::Red), "default");
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
