//! User `[keybinds]` overrides layered over [`BINDINGS`].
//!
//! An override replaces an action's chords in both hotkey modes. An entry that names an
//! unknown action, holds an unparseable chord, or collides with another key the home view
//! or live mode already handles is dropped with its own warning, and its action keeps the
//! shipped chords. Conflicts are checked for the active hotkey mode against the merged
//! result, so moving two actions onto each other's keys is accepted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use crossterm::event::KeyCode;

use super::bindings::{
    action_by_name, chord_from_parts, default_chords, format_chord, palette_id, parse_chord,
    ActionId, Binding, Chord, BINDINGS,
};

/// Accepted overrides, keyed by action.
#[derive(Debug, Default)]
pub struct Keymap {
    overrides: HashMap<ActionId, Vec<Chord>>,
}

impl Keymap {
    /// The binding's chords in the given mode: the override when one was accepted.
    pub(super) fn chords<'a>(&'a self, b: &'a Binding, strict: bool) -> &'a [Chord] {
        self.overrides
            .get(&b.id)
            .map_or_else(|| default_chords(b, strict), Vec::as_slice)
    }

    pub(super) fn is_overridden(&self, id: ActionId) -> bool {
        self.overrides.contains_key(&id)
    }
}

/// An ignored `[keybinds]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryWarning {
    pub name: String,
    pub reason: String,
}

impl std::fmt::Display for EntryWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[keybinds] {}: {}; keeping the default keys",
            self.name, self.reason
        )
    }
}

/// One action as the read-only settings page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub name: &'static str,
    pub title: &'static str,
    /// Effective keys in the active mode, `", "`-joined; empty when unbound.
    pub keys: String,
    pub default_keys: String,
    pub overridden: bool,
}

/// Every action in table order with the keys `config` gives it, plus the entry warnings.
pub fn describe(config: &crate::session::Config) -> (Vec<ActionRow>, Vec<EntryWarning>) {
    let strict = config.session.strict_hotkeys;
    let (km, warnings) = from_config(config);
    let join = |chords: &[Chord]| {
        let mut labels: Vec<String> = Vec::new();
        for label in chords.iter().map(format_chord) {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        labels.join(", ")
    };
    let rows = BINDINGS
        .iter()
        .map(|b| ActionRow {
            name: palette_id(b.id),
            title: b
                .palette
                .as_ref()
                .map(|p| p.title)
                .or(b.help.as_ref().map(|h| h.desc))
                .unwrap_or(""),
            keys: join(km.chords(b, strict)),
            default_keys: join(default_chords(b, strict)),
            overridden: km.is_overridden(b.id),
        })
        .collect();
    (rows, warnings)
}

/// Keys outside [`BINDINGS`] that an override could shadow.
#[derive(Debug, Default)]
pub struct Reserved {
    pub leader: Option<Chord>,
    pub exit: Vec<Chord>,
    /// `(tool name, hotkey)`.
    pub tools: Vec<(String, Chord)>,
}

impl Reserved {
    pub fn from_config(config: &crate::session::Config) -> Self {
        let to_chord = |(code, mods)| chord_from_parts(code, mods);
        Self {
            leader: super::live_send::resolve_leader(&config.session.live_send_leader)
                .map(to_chord),
            exit: super::live_send::parse_chord_list(&config.session.live_send_exit_chord)
                .into_iter()
                .map(to_chord)
                .collect(),
            tools: super::input::build_tool_hotkey_cache(&config.tools)
                .into_iter()
                .map(|(name, code, mods)| (name, to_chord((code, mods))))
                .collect(),
        }
    }
}

/// The keymap and per-entry warnings for a config's `[keybinds]` table.
pub fn from_config(config: &crate::session::Config) -> (Keymap, Vec<EntryWarning>) {
    build(
        &config.keybinds,
        config.session.strict_hotkeys,
        &Reserved::from_config(config),
    )
}

/// Bare keys the home view's navigation arms handle after the registry, which an
/// override would take from them. Ctrl+C quits from `App` before the home view runs.
fn navigation_owner(c: &Chord) -> Option<&'static str> {
    if c.ctrl && !c.alt && c.code == KeyCode::Char('c') {
        return Some("quit");
    }
    if c.ctrl || c.alt {
        return None;
    }
    matches!(
        c.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Esc
            | KeyCode::Char('j' | 'k' | 'h' | 'l' | 'G' | '{' | '}' | '<' | '>')
    )
    .then_some("navigation")
}

/// Live mode checks these before relaying a key to the Agent.
fn is_live_action(id: ActionId) -> bool {
    matches!(
        id,
        ActionId::JumpPrevFinished
            | ActionId::JumpNextFinished
            | ActionId::PreviewScrollUp
            | ActionId::PreviewScrollDown
    )
}

/// Whether one key event can match both chords. An unset `shift` also matches the shifted
/// event (see `chord_matches`), so Shift never separates two chords.
fn overlaps(a: &Chord, b: &Chord) -> bool {
    a.code == b.code && a.ctrl == b.ctrl && a.alt == b.alt
}

/// Why `c` cannot be bound to `id`, from rules that do not depend on other overrides.
fn fixed_conflict(id: ActionId, c: &Chord, reserved: &Reserved) -> Option<String> {
    let key = format_chord(c);
    if let Some(owner) = navigation_owner(c) {
        return Some(format!("{key} is reserved for {owner}"));
    }
    if let Some((tool, _)) = reserved.tools.iter().find(|(_, t)| overlaps(t, c)) {
        return Some(format!("{key} is the hotkey for tool {tool}"));
    }
    if id == ActionId::Quit && (c.ctrl || c.alt) {
        return Some(format!("{key}: quit takes a key without Ctrl or Alt"));
    }
    if is_live_action(id) {
        if matches!(c.code, KeyCode::Char(_)) && !c.ctrl && !c.alt {
            return Some(format!(
                "{key}: live mode relays typing, so a character key needs Ctrl or Alt"
            ));
        }
        if reserved.leader.as_ref().is_some_and(|l| overlaps(l, c)) {
            return Some(format!("{key} is the live-send leader"));
        }
        if reserved.exit.iter().any(|e| overlaps(e, c)) {
            return Some(format!("{key} exits live mode"));
        }
    }
    None
}

/// Apply `entries` over the shipped table, validating against `strict` mode.
pub fn build(
    entries: &BTreeMap<String, Vec<String>>,
    strict: bool,
    reserved: &Reserved,
) -> (Keymap, Vec<EntryWarning>) {
    let mut warnings = Vec::new();
    let mut reject = |name: &str, reason: String| warn(&mut warnings, name, reason);

    let mut candidates: BTreeMap<String, (ActionId, Vec<Chord>)> = BTreeMap::new();
    'entries: for (name, specs) in entries {
        let Some(id) = action_by_name(name) else {
            reject(name, "unknown action".to_string());
            continue;
        };
        let defaults = default_chords(binding(id), strict);
        let mut chords: Vec<Chord> = Vec::new();
        for spec in specs {
            let Some(c) = parse_chord(spec) else {
                reject(name, format!("cannot parse {spec:?}"));
                continue 'entries;
            };
            // A shipped chord kept in an override is never a new conflict.
            if !defaults.contains(&c) {
                if let Some(reason) = fixed_conflict(id, &c, reserved) {
                    reject(name, reason);
                    continue 'entries;
                }
            }
            if !chords.contains(&c) {
                chords.push(c);
            }
        }
        candidates.insert(name.clone(), (id, chords));
    }

    // Dropping an entry restores its defaults, which can collide with another
    // override, so repeat until a pass drops nothing.
    loop {
        let merged = Keymap {
            overrides: candidates.values().cloned().collect(),
        };
        let mut dropped = Vec::new();
        for (name, (id, chords)) in &candidates {
            let defaults = default_chords(binding(*id), strict);
            let clash = chords
                .iter()
                .filter(|c| !defaults.contains(c))
                .find_map(|c| {
                    BINDINGS
                        .iter()
                        .filter(|other| other.id != *id)
                        .find(|other| merged.chords(other, strict).iter().any(|o| overlaps(o, c)))
                        .map(|other| (c, other.id))
                });
            if let Some((c, other)) = clash {
                dropped.push((
                    name.clone(),
                    format!("{} is bound to {}", format_chord(c), palette_id(other)),
                ));
            }
        }
        if dropped.is_empty() {
            return (merged, warnings);
        }
        for (name, reason) in dropped {
            candidates.remove(&name);
            warn(&mut warnings, &name, reason);
        }
    }
}

fn warn(warnings: &mut Vec<EntryWarning>, name: &str, reason: String) {
    let warning = EntryWarning {
        name: name.to_string(),
        reason,
    };
    tracing::warn!("{warning}");
    warnings.push(warning);
}

fn binding(id: ActionId) -> &'static Binding {
    BINDINGS
        .iter()
        .find(|b| b.id == id)
        .expect("every ActionId named by the table has a binding")
}

/// Process-wide keymap the dispatcher, help, palette and footer read, replaced whenever the
/// home view applies config. Empty until then, so other processes see the shipped table.
static ACTIVE: RwLock<Option<Arc<Keymap>>> = RwLock::new(None);

pub fn install(keymap: Keymap) {
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(keymap));
}

pub(super) fn with_active<R>(f: impl FnOnce(&Keymap) -> R) -> R {
    let active = ACTIVE.read().unwrap_or_else(|e| e.into_inner()).clone();
    match active {
        Some(km) => f(&km),
        None => f(&Keymap::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn reserved() -> Reserved {
        Reserved {
            leader: parse_chord("C-b"),
            exit: vec![parse_chord("C-q").unwrap()],
            tools: vec![("lazygit".to_string(), parse_chord("M-g").unwrap())],
        }
    }

    fn chords(km: &Keymap, id: ActionId, strict: bool) -> Vec<String> {
        km.chords(binding(id), strict)
            .iter()
            .map(format_chord)
            .collect()
    }

    #[test]
    fn accepted_overrides_replace_both_modes_and_empty_unbinds() {
        let (km, warnings) = build(
            &entries(&[
                ("delete", &["C-x", "F2"]),
                ("dismiss-update-banner", &["M-x"]),
                ("previous-finished", &["M-k"]),
                ("next-finished", &["M-j"]),
                ("stop", &[]),
                ("projects", &["p", "F3"]),
            ]),
            false,
            &reserved(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        for strict in [false, true] {
            assert_eq!(chords(&km, ActionId::Delete, strict), ["Ctrl+X", "F2"]);
            assert!(chords(&km, ActionId::Stop, strict).is_empty());
        }
        // The shipped `p` shared with the project-pin toggle is kept, not a conflict.
        assert_eq!(chords(&km, ActionId::Projects, false), ["p", "F3"]);
        assert!(km.is_overridden(ActionId::Delete));
        assert!(!km.is_overridden(ActionId::Diff));
        assert_eq!(chords(&km, ActionId::Diff, false), ["D"]);
    }

    #[test]
    fn each_bad_entry_warns_and_keeps_its_defaults() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("no-such-action", &["F2"], "unknown action"),
            ("delete", &["F2", "Ctrl+K+J"], "cannot parse \"Ctrl+K+J\""),
            ("delete", &["x"], "x is bound to stop"),
            ("delete", &["C-k"], "Ctrl+K is bound to command-palette"),
            ("delete", &["j"], "j is reserved for navigation"),
            ("delete", &["Enter"], "Enter is reserved for navigation"),
            ("delete", &["C-c"], "Ctrl+C is reserved for quit"),
            ("delete", &["M-g"], "Alt+G is the hotkey for tool lazygit"),
            (
                "delete",
                &["C-x"],
                "Ctrl+X is bound to dismiss-update-banner",
            ),
            ("quit", &["C-w"], "quit takes a key without Ctrl or Alt"),
            ("next-finished", &["S-x"], "live mode relays typing"),
            ("next-finished", &["C-b"], "Ctrl+B is the live-send leader"),
            ("preview-scroll-up", &["C-q"], "Ctrl+Q exits live mode"),
        ];
        for (name, specs, want) in cases {
            let (km, warnings) = build(&entries(&[(name, specs)]), false, &reserved());
            assert_eq!(warnings.len(), 1, "{name} {specs:?}: {warnings:?}");
            assert_eq!(warnings[0].name, *name);
            assert!(
                warnings[0].reason.contains(want),
                "{name} {specs:?}: {warnings:?}"
            );
            if let Some(id) = action_by_name(name) {
                assert!(!km.is_overridden(id), "{name}");
            }
        }
    }

    #[test]
    fn conflicts_are_checked_against_the_merged_result_in_the_active_mode() {
        // A swap is valid once both sides move.
        let (km, warnings) = build(
            &entries(&[("delete", &["x"]), ("stop", &["d"])]),
            false,
            &reserved(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(chords(&km, ActionId::Stop, false), ["d"]);

        // `d` is free in strict mode, where delete is Shift+D, and taken otherwise.
        let one = entries(&[("stop", &["d"])]);
        assert!(build(&one, true, &reserved()).1.is_empty());
        assert_eq!(build(&one, false, &reserved()).1.len(), 1);

        // Two overrides onto one chord both drop, and the default each restores is
        // checked again: rename's `r` returns and now clashes with diff's override.
        let (km, warnings) = build(
            &entries(&[("rename", &["F2"]), ("stop", &["F2"]), ("diff", &["r"])]),
            false,
            &reserved(),
        );
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(
            warnings[2].to_string(),
            "[keybinds] diff: r is bound to rename; keeping the default keys"
        );
        assert!(km.overrides.is_empty());
    }

    #[test]
    fn overrides_drive_resolution_and_labels() {
        use super::super::bindings::{label_in, resolve_in, Ctx};
        use crossterm::event::{KeyEvent, KeyModifiers};
        let (km, warnings) = build(
            &entries(&[("delete", &["C-e"]), ("restart", &[])]),
            false,
            &reserved(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let ctx = Ctx {
            view_mode: super::super::ViewMode::Structured,
            sort_order: crate::session::config::SortOrder::Newest,
            has_search: false,
            project_group_selected: false,
            preview_scroll: false,
        };
        for (key, want) in [
            (
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                Some(ActionId::Delete),
            ),
            (KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), None),
            (KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), None),
        ] {
            assert_eq!(resolve_in(&km, &key, false, &ctx), want, "{key:?}");
        }
        assert_eq!(label_in(&km, ActionId::Delete, true), "Ctrl+E");
        assert_eq!(label_in(&km, ActionId::Restart, false), "");
    }
}
