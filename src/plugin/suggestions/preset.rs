//! The start preset of a suggestion kind: what Start creates a Session with.

use super::*;
use crate::session::config::Config;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PresetView {
    Structured,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preset {
    pub agent: String,
    pub view: PresetView,
    pub worktree: bool,
    pub sandbox: bool,
    pub yolo: bool,
    /// A template over the item's variables. `None` uses the prompt the
    /// plugin published with each item.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// The new-Session defaults of `config`, with the plugin's own prompt.
pub(crate) fn default_preset(config: &Config) -> Preset {
    let agent = config
        .session
        .default_tool
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    // Same rule as the web wizard; the spawn fills model and effort from `acp_defaults`.
    let structured = matches!(
        config.acp.structured_view,
        StructuredView::Auto | StructuredView::Structured
    ) && crate::session::builder::structured::tool_acp_capable(&agent, config);
    Preset {
        view: if structured {
            PresetView::Structured
        } else {
            PresetView::Terminal
        },
        worktree: config.worktree.enabled,
        sandbox: config.sandbox.enabled_by_default,
        yolo: config.session.yolo_mode_default,
        prompt: None,
        agent,
    }
}

/// The stored preset of a kind, or `None` while it follows the defaults. An
/// undecodable row is logged and treated as absent, as [`presets`] does.
pub(crate) fn preset(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
) -> anyhow::Result<Option<Preset>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT preset_json FROM suggestion_presets
             WHERE profile = ?1 AND provider_id = ?2 AND criterion = ?3",
            params![profile, provider_id, criterion],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json.and_then(|j| match serde_json::from_str(&j) {
        Ok(preset) => Some(preset),
        Err(e) => {
            tracing::warn!(
                target: "plugin.suggestions",
                provider = %provider_id,
                criterion = %criterion,
                "ignoring undecodable stored preset: {e}"
            );
            None
        }
    }))
}

pub(crate) fn set_preset(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
    preset: &Preset,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO suggestion_presets (profile, provider_id, criterion, preset_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (profile, provider_id, criterion) DO UPDATE SET preset_json = ?4",
        params![
            profile,
            provider_id,
            criterion,
            serde_json::to_string(preset)?
        ],
    )?;
    Ok(())
}

pub(crate) fn delete_preset(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM suggestion_presets
         WHERE profile = ?1 AND provider_id = ?2 AND criterion = ?3",
        params![profile, provider_id, criterion],
    )?;
    Ok(())
}

/// Every stored preset of the profile as `(provider_id, criterion, preset)`.
/// An undecodable row is logged and skipped, so the kind falls back to its defaults.
pub(crate) fn presets(
    conn: &Connection,
    profile: &str,
) -> rusqlite::Result<Vec<(String, String, Preset)>> {
    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT provider_id, criterion, preset_json FROM suggestion_presets
             WHERE profile = ?1 ORDER BY provider_id, criterion",
        )?
        .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(
            |(provider_id, criterion, json)| match serde_json::from_str(&json) {
                Ok(preset) => Some((provider_id, criterion, preset)),
                Err(e) => {
                    tracing::warn!(
                        target: "plugin.suggestions",
                        provider = %provider_id,
                        criterion = %criterion,
                        "ignoring undecodable stored preset: {e}"
                    );
                    None
                }
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn an_undecodable_stored_preset_reads_as_absent() {
        let conn = store();
        conn.execute(
            "INSERT INTO suggestion_presets (profile, provider_id, criterion, preset_json)
             VALUES ('p', 'acme.github', 'assigned', '{\"agent\": 7}')",
            [],
        )
        .unwrap();
        assert_eq!(preset(&conn, "p", "acme.github", "assigned").unwrap(), None);
        assert!(presets(&conn, "p").unwrap().is_empty());
    }
}
