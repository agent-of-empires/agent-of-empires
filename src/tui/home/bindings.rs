//! Single source of truth for home-view action keybindings.
//!
//! Every relocatable action (one that binds to a different chord in strict vs
//! non-strict mode) is declared exactly once in [`BINDINGS`]. The dispatcher
//! ([`resolve`]), the command palette, and the help overlay all derive from
//! this table, so a binding can no longer drift between those surfaces.
//!
//! Pure navigation keys (arrows, `j`/`k`/`h`/`l`, Home/End, PageUp/Down,
//! `{`/`}`, `<`/`>`, Enter, Tab) are NOT here: they never relocate between
//! modes and were never the source of the strict-mode bugs. They stay as
//! explicit arms in `dispatch_action_key`, tried after this table.
//!
//! ## Strict-mode relocation rule
//!
//! In strict mode bare lowercase letters are reserved for the typing-guard, so
//! actions move under a modifier. The consistent rule, applied uniformly:
//!   - the bare-lowercase (primary) action  -> `Shift`+letter
//!   - the `Shift`+letter (secondary) action -> `Ctrl`+letter
//!
//! e.g. `d`=delete / `Shift+D`=diff (non-strict) become `Shift+D`=delete /
//! `Ctrl+D`=diff (strict). `p`=projects / `Shift+P`=profiles likewise become
//! `Shift+P`=projects / `Ctrl+P`=profiles.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::ViewMode;
use crate::session::config::SortOrder;
use crate::tui::dialogs::PaletteGroup;

/// Logical action. Each variant is dispatched by `HomeView::run_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    Quit,
    Help,
    ResolveIndeterminate,
    ToolPicker,
    SearchStart,
    SearchNext,
    NewSession,
    NewFromSelection,
    NewFromProject,
    AttachTerminal,
    ToggleView,
    SendMessage,
    RespondToPermission,
    Stop,
    Delete,
    Rename,
    SetWorktreeName,
    AddProject,
    Diff,
    Serve,
    Settings,
    Profiles,
    Projects,
    Restart,
    Update,
    ToggleArchive,
    ToggleFavorite,
    MoveRowUp,
    MoveRowDown,
    JumpPrevFinished,
    JumpNextFinished,
    ToggleSnooze,
    /// Toggle the selected session's unread marker (read -> manual-unread;
    /// unread -> read). Gated behind the `session.unread_indicator` config
    /// toggle (on by default); a no-op when disabled.
    ToggleUnread,
    ToggleContainer,
    TogglePreviewInfo,
    /// Toggle the system diagnostics strip (live CPU and memory pressure plus
    /// running agent/process counts). Persisted via
    /// `session.show_diagnostics_pane` so it survives restarts.
    ToggleDiagnostics,
    ToggleHideStopped,
    /// Open the read-only host and running-agent resource view.
    OpenSystemHealth,
    SortPicker,
    GroupBy,
    NextWaiting,
    /// Open the plugin manager (palette only; no default chord).
    Plugins,
    /// Open the skills manager (palette only; no default chord).
    Skills,
    /// Pin or unpin the selected project header (project view only). Pinning
    /// registers the repo so the project persists in the view without any
    /// sessions; unpinning removes the registry entry.
    ToggleProjectPin,
    /// Open the tips overlay (the browsable list from `crate::tips`). Has no
    /// global hotkey on purpose; reached from the command palette, the tips
    /// badge, and the `?` help screen, so it doesn't consume a scarce key.
    Tips,
    /// Fork the selected session into a new independent session that resumes
    /// its conversation context (palette + context menu only; no chord).
    Fork,
    /// Run the agent-driven "Auto-name now" one-shot for the selected
    /// still-default-named session, on demand and even when auto-rename-on-start
    /// is disabled (#3039). Both session backends use the canonical daemon command.
    AutoName,
    /// Permanently purge every trashed session after a confirm (palette only; no chord).
    EmptyTrash,
    ToggleSplitTerminal,
    /// Open the command palette (any hotkey mode).
    OpenCommandPalette,
    /// Dismiss the update, status or image banner for this session. `App` owns the
    /// banner state, so it executes this before the home view sees the key.
    DismissUpdateBanner,
    /// Scroll the preview back or forward a page.
    PreviewScrollUp,
    PreviewScrollDown,
}

impl ActionId {
    /// Whether acting on this variant needs a canonical session behind the
    /// cursor. The creating stub is a sidebar placeholder the daemon owns end
    /// to end: it has neither a runtime nor a published row, so these actions
    /// must refuse it instead of sending a mutation for an unknown id. Actions
    /// listing their own status gate (`AttachTerminal` and live-send already
    /// refuse `Status::Creating`) still belong here: the fence is per action,
    /// not per status check.
    pub const fn needs_canonical_session(self) -> bool {
        matches!(
            self,
            Self::AttachTerminal
                | Self::ToggleView
                | Self::SendMessage
                | Self::RespondToPermission
                | Self::Stop
                | Self::Delete
                | Self::Rename
                | Self::SetWorktreeName
                | Self::AddProject
                | Self::Diff
                | Self::Restart
                | Self::ToggleArchive
                | Self::ToggleFavorite
                | Self::ToggleSnooze
                | Self::ToggleUnread
                | Self::ToggleContainer
                | Self::Fork
                | Self::AutoName
                | Self::NewFromSelection
        )
    }
}

/// A single chord. `ctrl` requires the Control modifier; Shift is implicit in
/// the uppercase letter `code` (terminals deliver `Shift+d` as `Char('D')`,
/// and iOS Mosh delivers a bare uppercase keycode with no Shift modifier, so
/// matching on the uppercase code rather than a Shift flag covers both). `shift` is the
/// exception for non-character keys (`Shift+PageUp`), which have no uppercase form; it is
/// ignored for `KeyCode::Char`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

const fn k(c: char) -> Chord {
    Chord {
        code: KeyCode::Char(c),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

const fn ctrl(c: char) -> Chord {
    Chord {
        code: KeyCode::Char(c),
        ctrl: true,
        alt: false,
        shift: false,
    }
}

const fn f(n: u8) -> Chord {
    Chord {
        code: KeyCode::F(n),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

const fn ctrl_code(code: KeyCode) -> Chord {
    Chord {
        code,
        ctrl: true,
        alt: false,
        shift: false,
    }
}

const fn alt_code(code: KeyCode) -> Chord {
    Chord {
        code,
        ctrl: false,
        alt: true,
        shift: false,
    }
}

const fn shift_code(code: KeyCode) -> Chord {
    Chord {
        code,
        ctrl: false,
        alt: false,
        shift: true,
    }
}

/// Contextual guard: a binding only resolves when its context holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Always,
    TerminalView,
    AttentionSort,
    /// Favorites are actionable: either the Attention sort is active, where
    /// favorite is a within-tier tiebreak, or `session.favorites_first` is on,
    /// where it pins in every sort order. With neither, toggling a favorite
    /// would have no visible effect, so the key stays inert.
    FavoritesUsable,
    SearchActive,
    /// The cursor is on a real (non-synthetic) project header in project view.
    ProjectGroupSelected,
    /// The unread-session feature is enabled (`session.unread_indicator`). When
    /// off, the binding is removed from dispatch so the key isn't swallowed by
    /// a dead action; help and the command palette skip it separately.
    UnreadEnabled,
    /// Rows can be arranged by hand. Under a computed sort the action still resolves, so
    /// the keys are not silently swallowed by cursor movement: it explains itself instead.
    CustomSortable,
    /// Preview scrolling is available for live-send and visible retained history.
    PreviewScroll,
    /// `App` dispatches the chord before the home view, so it never resolves here and the
    /// key still falls through to plugin bindings when `App` declines it.
    AppOnly,
}

/// Help-overlay section. Ordering mirrors `components/help.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpSection {
    Actions,
    Attention,
    Views,
    Other,
}

pub struct HelpMeta {
    pub section: HelpSection,
    pub desc: &'static str,
}

pub struct PaletteMeta {
    pub title: &'static str,
    pub keywords: &'static [&'static str],
    pub group: PaletteGroup,
}

pub struct Binding {
    pub id: ActionId,
    pub non_strict: &'static [Chord],
    pub strict: &'static [Chord],
    pub context: Context,
    pub help: Option<HelpMeta>,
    pub palette: Option<PaletteMeta>,
}

/// Runtime state the guards consult.
pub struct Ctx {
    pub view_mode: ViewMode,
    pub sort_order: SortOrder,
    pub has_search: bool,
    /// True when the cursor sits on a real project header in project view, so
    /// the pin toggle can claim its chord ahead of the projects-dialog binding.
    pub project_group_selected: bool,
    pub preview_scroll: bool,
}

fn chord_matches(c: &Chord, key: &KeyEvent) -> bool {
    // Match the Ctrl and Alt modifiers exactly: a bare chord must NOT fire
    // when Ctrl or Alt is held. Otherwise `k('q')` would also match Ctrl+Q
    // (reserved for exiting live-send mode, #1569) and `k('d')` would match
    // Ctrl+D, letting a modified chord trigger a bare-letter action. The Alt
    // term is what keeps Alt+Up/Alt+Down (the custom-sort reorder chords) from
    // also firing on the unmodified arrows.
    key.code == c.code
        && key.modifiers.contains(KeyModifiers::CONTROL) == c.ctrl
        && key.modifiers.contains(KeyModifiers::ALT) == c.alt
        && (!c.shift || key.modifiers.contains(KeyModifiers::SHIFT))
}

fn context_holds(context: Context, ctx: &Ctx) -> bool {
    match context {
        Context::Always => true,
        Context::TerminalView => ctx.view_mode == ViewMode::Terminal,
        Context::AttentionSort => ctx.sort_order == SortOrder::Attention,
        Context::FavoritesUsable => {
            ctx.sort_order == SortOrder::Attention || crate::session::favorites_first()
        }
        Context::SearchActive => ctx.has_search,
        Context::ProjectGroupSelected => ctx.project_group_selected,
        Context::UnreadEnabled => crate::session::unread_enabled(),
        Context::CustomSortable => true,
        Context::PreviewScroll => ctx.preview_scroll,
        Context::AppOnly => false,
    }
}

/// Resolve a key event to an action, honoring strict mode and context guards.
/// Returns the first matching binding in table order; context-guarded entries
/// are listed before the unguarded entries that share their chord.
pub fn resolve(key: &KeyEvent, strict: bool, ctx: &Ctx) -> Option<ActionId> {
    for b in BINDINGS {
        let chords = if strict { b.strict } else { b.non_strict };
        if context_holds(b.context, ctx) && chords.iter().any(|c| chord_matches(c, key)) {
            return Some(b.id);
        }
    }
    None
}

/// A plugin-contributed action a key resolved to: a plugin id plus the command
/// the keybind targets. At Tier 0 there is no executor, so resolving one is
/// inspectable (and surfaces a "needs runtime" notice) but not yet runnable;
/// the executor lands with the runtime host (#2095).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAction {
    pub plugin_id: String,
    pub action: String,
}

impl PluginAction {
    /// Canonical external name, `plugin.<id>.<action>`. Idempotent: a manifest
    /// keybind may already target a fully-qualified `plugin.<id>.<cmd>` command,
    /// so an action that is already canonical is returned unchanged rather than
    /// double-prefixed.
    pub fn canonical(&self) -> String {
        if self.action.starts_with("plugin.") {
            return self.action.clone();
        }
        format!("plugin.{}.{}", self.plugin_id, self.action)
    }
}

/// The merged resolver's result: a core action, or a plugin action. Core always
/// shadows a plugin binding on the same chord (core is resolved first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAction {
    Core(ActionId),
    Plugin(PluginAction),
}

/// Resolve a key across the merged core + plugin binding tables. Core bindings
/// (the static [`BINDINGS`] table, honoring strict mode and context) are tried
/// first and always win; only then are active plugins' declared keybinds
/// consulted. Returns `None` if nothing claims the chord.
pub fn resolve_action(key: &KeyEvent, strict: bool, ctx: &Ctx) -> Option<ResolvedAction> {
    if let Some(id) = resolve(key, strict, ctx) {
        return Some(ResolvedAction::Core(id));
    }
    for (chord, action) in plugin_bindings() {
        if chord_matches(&chord, key) {
            return Some(ResolvedAction::Plugin(action));
        }
    }
    None
}

/// Whether a plugin-declared keybind string (e.g. `Ctrl+Shift+G`) matches this
/// key event. The structured view resolves daemon-provided command keybinds
/// through this rather than the local registry, so it parses the raw chord
/// string here. A chord string that does not parse never matches.
///
pub fn keybind_matches(key_str: &str, key: &KeyEvent) -> bool {
    parse_chord(key_str).is_some_and(|chord| chord_matches(&chord, key))
}

/// The active plugins' declared keybinds, parsed into `(chord, action)`. A
/// keybind whose key string does not parse is skipped (its conflict-free state
/// is surfaced by `aoe plugin info`).
// ponytail: rebuilt per unmatched keypress; the active set is tiny and this is
// not a hot path. Cache behind the registry generation if that ever changes.
fn plugin_bindings() -> Vec<(Chord, PluginAction)> {
    let mut out = Vec::new();
    for p in crate::plugin::registry().active() {
        for kb in &p.manifest.keybinds {
            if let Some(chord) = parse_chord(&kb.key) {
                out.push((
                    chord,
                    PluginAction {
                        plugin_id: p.id().to_string(),
                        action: kb.command.clone(),
                    },
                ));
            }
        }
    }
    out
}

/// Parse a chord in the tmux-style grammar shared with `live_send_leader` (`C-k`, `Ctrl+K`,
/// `M-Up`, `S-PageUp`, `F5`, `q`) into a [`Chord`]. Bare letters are lowercase and
/// `Shift`+letter is the uppercase char, as in the table; Shift on a named key sets `shift`.
/// Returns `None` for anything the live-send grammar rejects.
pub fn parse_chord(s: &str) -> Option<Chord> {
    let (code, mods) = super::live_send::parse_chord(s)?;
    let shift = mods.contains(KeyModifiers::SHIFT);
    let code = match code {
        KeyCode::Char(c) if shift => KeyCode::Char(c.to_ascii_uppercase()),
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    Some(Chord {
        code,
        ctrl: mods.contains(KeyModifiers::CONTROL),
        alt: mods.contains(KeyModifiers::ALT),
        shift: shift && !matches!(code, KeyCode::Char(_)),
    })
}

/// Whether a core binding already claims `chord` in either mode. Used by
/// `aoe plugin info` to flag a plugin keybind that core shadows.
pub fn core_shadows(chord: &Chord) -> bool {
    BINDINGS.iter().any(|b| {
        // App-only chords run before the home view and only while a banner shows, so they
        // do not statically shadow a plugin chord.
        if b.context == Context::AppOnly {
            return false;
        }
        b.non_strict.iter().chain(b.strict).any(|c| {
            // Same rule as `chord_matches`: an unset core `shift` also matches shifted events.
            c.code == chord.code
                && c.ctrl == chord.ctrl
                && c.alt == chord.alt
                && (!c.shift || chord.shift)
        })
    })
}

/// Human-readable label for a binding's primary chord in the given mode, e.g.
/// `"D"`, `"Ctrl+D"`, `"F5"`. Returns `""` if the action has no binding in the
/// requested mode (e.g. `NextWaiting` in strict).
pub fn label(id: ActionId, strict: bool) -> String {
    let Some(b) = BINDINGS.iter().find(|b| b.id == id) else {
        return String::new();
    };
    let chords = if strict { b.strict } else { b.non_strict };
    chords.first().map(format_chord).unwrap_or_default()
}

/// Whether `key` is one of the action's chords in the given mode, ignoring context
/// guards. For handlers that run outside the home dispatcher.
pub fn is_bound(id: ActionId, strict: bool, key: &KeyEvent) -> bool {
    BINDINGS.iter().any(|b| {
        let chords = if strict { b.strict } else { b.non_strict };
        b.id == id && chords.iter().any(|c| chord_matches(c, key))
    })
}

/// The key event a click on the action's footer button synthesizes: its primary chord.
pub fn key_event(id: ActionId, strict: bool) -> Option<KeyEvent> {
    let b = BINDINGS.iter().find(|b| b.id == id)?;
    let c = if strict { b.strict } else { b.non_strict }.first()?;
    let mut modifiers = KeyModifiers::NONE;
    if c.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if c.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if c.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    Some(KeyEvent::new(c.code, modifiers))
}

fn format_chord(c: &Chord) -> String {
    let key = match c.code {
        KeyCode::Char(ch) => ch.to_ascii_uppercase().to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        _ => return String::new(),
    };
    // A bare letter keeps its lone-character form; every modified chord is spelled out, or
    // the help overlay and the palette show the new arrow bindings with no key at all.
    if c.shift {
        return format!("Shift+{key}");
    }
    match (c.ctrl, c.alt, c.code) {
        (false, false, KeyCode::Char(ch)) => ch.to_string(),
        (true, true, _) => format!("Ctrl+Alt+{key}"),
        (true, false, _) => format!("Ctrl+{key}"),
        (false, true, _) => format!("Alt+{key}"),
        (false, false, _) => key,
    }
}

// Order matters: context-guarded bindings that share a chord with an unguarded
// one (search-cycle vs new, etc.) come first so they win when their guard holds.
pub static BINDINGS: &[Binding] = &[
    // --- search cycle (only while matches are active; both modes) ---
    // Only bare `n` cycles, forward and wrapping, so every match stays reachable with no
    // reverse binding; `N` stays a new-session key in every state so a committed search
    // never shadows it (#3038).
    Binding {
        id: ActionId::SearchNext,
        non_strict: &[k('n')],
        strict: &[k('n')],
        context: Context::SearchActive,
        help: None,
        palette: None,
    },
    // --- manual row ordering (Custom sort) ---
    Binding {
        id: ActionId::MoveRowUp,
        non_strict: &[ctrl_code(KeyCode::Up)],
        strict: &[ctrl_code(KeyCode::Up)],
        context: Context::CustomSortable,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Move row up (Custom sort)",
        }),
        palette: Some(PaletteMeta {
            title: "Move row up",
            keywords: &["order", "reorder", "move", "up"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::MoveRowDown,
        non_strict: &[ctrl_code(KeyCode::Down)],
        strict: &[ctrl_code(KeyCode::Down)],
        context: Context::CustomSortable,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Move row down (Custom sort)",
        }),
        palette: Some(PaletteMeta {
            title: "Move row down",
            keywords: &["order", "reorder", "move", "down"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::JumpPrevFinished,
        non_strict: &[alt_code(KeyCode::Up)],
        strict: &[alt_code(KeyCode::Up)],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Previous just-finished session",
        }),
        palette: Some(PaletteMeta {
            title: "Previous just-finished session",
            keywords: &["finished", "done", "idle", "jump", "prev"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::JumpNextFinished,
        non_strict: &[alt_code(KeyCode::Down)],
        strict: &[alt_code(KeyCode::Down)],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Next just-finished session",
        }),
        palette: Some(PaletteMeta {
            title: "Next just-finished session",
            keywords: &["finished", "done", "idle", "jump", "next"],
            group: PaletteGroup::Actions,
        }),
    },
    // Shift separates preview paging from agent input and sidebar paging.
    Binding {
        id: ActionId::PreviewScrollUp,
        non_strict: &[shift_code(KeyCode::PageUp)],
        strict: &[shift_code(KeyCode::PageUp)],
        context: Context::PreviewScroll,
        help: None,
        palette: None,
    },
    Binding {
        id: ActionId::PreviewScrollDown,
        non_strict: &[shift_code(KeyCode::PageDown)],
        strict: &[shift_code(KeyCode::PageDown)],
        context: Context::PreviewScroll,
        help: None,
        palette: None,
    },
    // --- attention-sort triage ---
    Binding {
        id: ActionId::ToggleFavorite,
        non_strict: &[k('f')],
        strict: &[k('F')],
        context: Context::FavoritesUsable,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Toggle favorite (pin to top)",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle favorite",
            keywords: &["star", "pin", "fav"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::ToggleSnooze,
        non_strict: &[k('h')],
        strict: &[k('H')],
        context: Context::AttentionSort,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Snooze (toggle, Attention sort)",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle snooze",
            keywords: &["later", "defer", "wait"],
            group: PaletteGroup::Actions,
        }),
    },
    // --- terminal-view only ---
    Binding {
        id: ActionId::ToggleContainer,
        non_strict: &[k('c')],
        strict: &[k('C')],
        context: Context::TerminalView,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Toggle container/host (sandbox)",
        }),
        palette: None,
    },
    // --- always-on actions ---
    Binding {
        id: ActionId::OpenCommandPalette,
        // `Ctrl+Shift+K` arrives as an uppercase char on some terminals.
        non_strict: &[ctrl('k'), ctrl('K')],
        strict: &[ctrl('k'), ctrl('K')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Command palette",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::DismissUpdateBanner,
        non_strict: &[ctrl('x')],
        strict: &[ctrl('x')],
        context: Context::AppOnly,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Dismiss update bar (this session)",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::Quit,
        non_strict: &[k('q')],
        strict: &[k('q')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Quit",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::Help,
        non_strict: &[k('?')],
        strict: &[k('?')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Toggle help",
        }),
        palette: Some(PaletteMeta {
            title: "Show keyboard shortcuts",
            keywords: &["keys", "shortcuts"],
            group: PaletteGroup::Settings,
        }),
    },
    Binding {
        id: ActionId::ToggleDiagnostics,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Toggle system health strip",
            keywords: &["system", "health", "memory", "cpu", "pressure"],
            group: PaletteGroup::Settings,
        }),
    },
    Binding {
        id: ActionId::OpenSystemHealth,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Open System Health",
            keywords: &["system", "health", "memory", "cpu", "agents", "processes"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::ToolPicker,
        non_strict: &[k(';')],
        strict: &[k(';')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Open tool session",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::SearchStart,
        non_strict: &[k('/')],
        strict: &[k('/')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Search",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::NewSession,
        non_strict: &[k('n')],
        strict: &[k('N')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "New session",
        }),
        palette: Some(PaletteMeta {
            title: "New session",
            keywords: &["create", "add", "spawn"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::NewFromSelection,
        non_strict: &[k('N')],
        strict: &[ctrl('n')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "New from selection",
        }),
        palette: Some(PaletteMeta {
            title: "New session from selection",
            keywords: &["create", "duplicate", "clone"],
            group: PaletteGroup::Actions,
        }),
    },
    // New session from a saved project. Follows the bare->Shift relocation
    // rule: `b` non-strict, `Shift+B` in strict.
    Binding {
        id: ActionId::NewFromProject,
        non_strict: &[k('b')],
        strict: &[k('B')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "New session from project",
        }),
        palette: Some(PaletteMeta {
            title: "New session from saved project",
            keywords: &["project", "saved", "registry", "create"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::AttachTerminal,
        non_strict: &[k('T')],
        strict: &[ctrl('t')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Attach to terminal",
        }),
        palette: Some(PaletteMeta {
            title: "Attach to paired terminal",
            keywords: &["shell", "host"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::ToggleView,
        non_strict: &[k('t')],
        strict: &[k('T')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Toggle Agent/Terminal view",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle Agent / Terminal view",
            keywords: &["switch", "shell"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::SendMessage,
        non_strict: &[k('m')],
        strict: &[k('M')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Send message to agent",
        }),
        palette: Some(PaletteMeta {
            title: "Send message to agent",
            keywords: &["prompt", "tell", "say"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::RespondToPermission,
        non_strict: &[k('a')],
        strict: &[k('A')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Respond to permission prompt",
        }),
        palette: Some(PaletteMeta {
            title: "Respond to permission prompt",
            keywords: &["allow", "deny", "approve", "permission"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::Stop,
        non_strict: &[k('x')],
        strict: &[k('X')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Stop session / kill terminal (by view)",
        }),
        palette: Some(PaletteMeta {
            title: "Stop session / kill terminal",
            keywords: &["kill", "end", "halt", "terminal"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::Delete,
        non_strict: &[k('d')],
        strict: &[k('D')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Delete session/group",
        }),
        palette: Some(PaletteMeta {
            title: "Delete session or group",
            keywords: &["remove", "trash"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::Rename,
        non_strict: &[k('r')],
        strict: &[k('R')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Rename session/group",
        }),
        palette: Some(PaletteMeta {
            title: "Rename or move to group",
            keywords: &["title", "label", "move", "regroup"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::SetWorktreeName,
        non_strict: &[k('W')],
        strict: &[ctrl('w')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Edit worktree workdir name",
        }),
        palette: Some(PaletteMeta {
            title: "Edit worktree workdir name",
            keywords: &["worktree", "workdir", "directory", "branch", "rename"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::AddProject,
        // No default key: the action is occasional and the obvious letters are
        // taken. Reachable from the command palette and the context menu.
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Add another project to this session",
        }),
        palette: Some(PaletteMeta {
            title: "Add project to this session",
            keywords: &["repo", "attach", "worktree", "multi-repo", "workspace"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::Diff,
        non_strict: &[k('D')],
        strict: &[ctrl('d')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Diff view (git changes)",
        }),
        palette: Some(PaletteMeta {
            title: "Open diff view",
            keywords: &["git", "changes"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::Serve,
        non_strict: &[k('R')],
        strict: &[ctrl('r')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: if cfg!(feature = "web") {
                "Connections: Web dashboard (browser / phone)"
            } else {
                "Connections: API server (LAN / Tunnel)"
            },
        }),
        palette: Some(PaletteMeta {
            title: if cfg!(feature = "web") {
                "Open Connections: Web dashboard (browser / phone)"
            } else {
                "Open Connections: API server (LAN / Tunnel)"
            },
            keywords: &[
                "connections",
                "daemon",
                "serve",
                "web",
                "dashboard",
                "browser",
                "remote",
                "phone",
                "mobile",
                "qr",
                "tunnel",
                "lan",
            ],
            group: PaletteGroup::Settings,
        }),
    },
    Binding {
        id: ActionId::Settings,
        non_strict: &[k('s')],
        strict: &[k('S')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Settings",
        }),
        palette: Some(PaletteMeta {
            title: "Open settings",
            keywords: &["preferences", "config"],
            group: PaletteGroup::Settings,
        }),
    },
    // Pin toggle shares `p` (Shift+P in strict) with Projects but fires only on a project
    // header, so it must precede the Projects binding. The help desc names that gate
    // ("group header only") because the `?` overlay has no notion of context and lists both
    // `p` rows unconditionally, which is what made the shared key look ambiguous in #3133.
    Binding {
        id: ActionId::ToggleProjectPin,
        non_strict: &[k('p')],
        strict: &[k('P')],
        context: Context::ProjectGroupSelected,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Pin/unpin project (group header only)",
        }),
        palette: None,
    },
    // P flip: Projects is the primary (bare `p`) action -> Shift+P in strict;
    // Profiles is the secondary (`Shift+P`) action -> Ctrl+P in strict.
    Binding {
        id: ActionId::Projects,
        non_strict: &[k('p')],
        strict: &[k('P')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Projects",
        }),
        palette: Some(PaletteMeta {
            title: "Manage projects",
            keywords: &["registry", "repos", "multi-repo", "workspace"],
            group: PaletteGroup::Settings,
        }),
    },
    Binding {
        id: ActionId::Profiles,
        non_strict: &[k('P')],
        strict: &[ctrl('p')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Profiles",
        }),
        palette: Some(PaletteMeta {
            title: "Switch profile",
            keywords: &["account", "switch"],
            group: PaletteGroup::Settings,
        }),
    },
    Binding {
        id: ActionId::Restart,
        non_strict: &[k('e'), f(5)],
        strict: &[k('E'), f(5)],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Restart session (also F5)",
        }),
        palette: Some(PaletteMeta {
            title: "Restart session",
            keywords: &["reload", "respawn", "reset"],
            group: PaletteGroup::Actions,
        }),
    },
    // `U` toggles read/unread, pinned to Shift+u in both modes (matching macOS Mail muscle
    // memory). It does not relocate under strict mode: a modified key already satisfies the
    // "no bare action letters" rule. `u` updates and relocates the usual way.
    Binding {
        id: ActionId::ToggleUnread,
        non_strict: &[k('U')],
        strict: &[k('U')],
        context: Context::UnreadEnabled,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Mark read/unread (toggle)",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle read/unread",
            keywords: &["read", "unread", "seen", "flag", "viewed"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::Update,
        non_strict: &[k('u')],
        strict: &[ctrl('u')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Update aoe (when available)",
        }),
        palette: None,
    },
    Binding {
        id: ActionId::ToggleArchive,
        non_strict: &[k('z')],
        strict: &[k('Z')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Archive (toggle, any sort)",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle archive",
            keywords: &["park", "stash", "done", "zzz"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::TogglePreviewInfo,
        non_strict: &[k('i')],
        strict: &[k('I')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Toggle preview info header",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle preview info header",
            keywords: &["hide", "show", "info", "header", "preview"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::ToggleSplitTerminal,
        non_strict: &[k('|')],
        strict: &[k('|')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Split preview: agent above terminal",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle split terminal preview",
            keywords: &["split", "terminal", "shell", "pane", "preview"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::ToggleHideStopped,
        non_strict: &[k('y')],
        strict: &[k('Y')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Hide stopped sessions in groups (toggle)",
        }),
        palette: Some(PaletteMeta {
            title: "Hide stopped sessions in groups",
            keywords: &["hide", "show", "stopped", "grey", "compact", "filter"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::SortPicker,
        // Shift+O sorts in both modes; bare `o` only outside strict.
        non_strict: &[k('o'), k('O'), ctrl('o')],
        strict: &[k('O'), ctrl('o')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Sort order",
        }),
        palette: Some(PaletteMeta {
            title: "Sort order",
            keywords: &["order", "sort", "pick"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::GroupBy,
        non_strict: &[k('g')],
        strict: &[ctrl('g')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Views,
            desc: "Group by",
        }),
        palette: Some(PaletteMeta {
            title: "Group by",
            keywords: &["group", "project", "pick"],
            group: PaletteGroup::Views,
        }),
    },
    Binding {
        id: ActionId::NextWaiting,
        non_strict: &[k('w')],
        strict: &[],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Jump to next waiting/idle",
        }),
        palette: Some(PaletteMeta {
            title: "Jump to next waiting / idle session",
            keywords: &["jump", "next", "waiting", "idle"],
            group: PaletteGroup::Views,
        }),
    },
    // Tips overlay. No key chords: it is reached from the palette, the badge and the `?`
    // screen, so it never shadows a typing-guard key. `help` is None because the overlay
    // skips keyless rows and gives it a bespoke row in `components/help.rs`.
    Binding {
        id: ActionId::Tips,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Show tips",
            keywords: &["tips", "hints", "learn", "discover", "did you know"],
            group: PaletteGroup::Settings,
        }),
    },
    // Palette-only: no default chord in either mode; the manager opens from
    // the command palette (or the web Settings Plugins tab).
    Binding {
        id: ActionId::Plugins,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Manage plugins",
            keywords: &["plugin", "extension", "enable", "disable"],
            group: PaletteGroup::Settings,
        }),
    },
    // Palette-only: no default chord in either mode; the manager opens from
    // the command palette (or the web Settings Skills tab).
    Binding {
        id: ActionId::Skills,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Manage skills",
            keywords: &["skill", "skills", "prompt"],
            group: PaletteGroup::Settings,
        }),
    },
    // Palette-only: Shift+F collides with strict-mode ToggleFavorite under Attention sort
    // and the home keyspace is saturated, so fork is reached from the palette and the
    // context menu.
    Binding {
        id: ActionId::Fork,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Fork session (resume context, diverge)",
            keywords: &["fork", "branch", "duplicate", "clone", "context", "resume"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::EmptyTrash,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Empty trash",
            keywords: &["trash", "empty", "purge", "delete", "clear"],
            group: PaletteGroup::Actions,
        }),
    },
    // The mnemonic keys (a/A, n/N, r/R, t/T) are taken and the home keyspace is saturated
    // (see Fork), so "Auto-name now" lands on the free v/V pair. Gated to a
    // still-default-named session by the daemon handler.
    Binding {
        id: ActionId::AutoName,
        non_strict: &[k('v')],
        strict: &[k('V')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Auto-name session now (agent one-shot)",
        }),
        palette: Some(PaletteMeta {
            title: "Auto-name now",
            keywords: &["rename", "title", "name", "auto", "smart", "generate"],
            group: PaletteGroup::Actions,
        }),
    },
    Binding {
        id: ActionId::ResolveIndeterminate,
        non_strict: &[],
        strict: &[],
        context: Context::Always,
        help: None,
        palette: Some(PaletteMeta {
            title: "Resolve unknown runtime change",
            keywords: &["resolve", "unknown", "quarantine", "unlock", "runtime"],
            group: PaletteGroup::Actions,
        }),
    },
];

/// Stable palette/test id for an action (matches the legacy `builtin_commands`
/// ids). Only actions that surface in the palette need one.
pub fn palette_id(id: ActionId) -> &'static str {
    match id {
        ActionId::NewSession => "new-session",
        ActionId::NewFromSelection => "new-from-selection",
        ActionId::NewFromProject => "new-from-project",
        ActionId::AttachTerminal => "attach-terminal",
        ActionId::ToggleView => "toggle-view",
        ActionId::SendMessage => "send-message",
        ActionId::RespondToPermission => "respond-to-permission",
        ActionId::Stop => "stop",
        ActionId::Delete => "delete",
        ActionId::Rename => "rename",
        ActionId::SetWorktreeName => "set-worktree-name",
        ActionId::AddProject => "add-project",
        ActionId::Diff => "diff",
        ActionId::Serve => "serve",
        ActionId::Settings => "settings",
        ActionId::Profiles => "profiles",
        ActionId::Projects => "projects",
        ActionId::Restart => "restart",
        ActionId::ToggleArchive => "archive",
        ActionId::ToggleFavorite => "favorite",
        ActionId::MoveRowUp => "move-row-up",
        ActionId::MoveRowDown => "move-row-down",
        ActionId::JumpPrevFinished => "previous-finished",
        ActionId::JumpNextFinished => "next-finished",
        ActionId::ToggleSnooze => "snooze",
        ActionId::ToggleUnread => "toggle-unread",
        ActionId::TogglePreviewInfo => "toggle-preview-info",
        ActionId::ToggleSplitTerminal => "toggle-split-terminal",
        ActionId::ToggleHideStopped => "toggle-hide-stopped",
        ActionId::ToggleDiagnostics => "toggle-diagnostics",
        ActionId::OpenSystemHealth => "open-system-health",
        ActionId::SortPicker => "pick-sort",
        ActionId::GroupBy => "pick-group-by",
        ActionId::Help => "help",
        ActionId::ResolveIndeterminate => "resolve-indeterminate",
        ActionId::NextWaiting => "next-waiting",
        ActionId::Quit => "quit",
        ActionId::ToolPicker => "tool-picker",
        ActionId::SearchStart => "search",
        ActionId::SearchNext => "search-next",
        ActionId::Update => "update",
        ActionId::ToggleContainer => "toggle-container",
        ActionId::ToggleProjectPin => "toggle-project-pin",
        ActionId::Tips => "tips",
        ActionId::Plugins => "plugins",
        ActionId::Skills => "skills",
        ActionId::Fork => "fork",
        ActionId::AutoName => "auto-name",
        ActionId::EmptyTrash => "empty-trash",
        ActionId::OpenCommandPalette => "command-palette",
        ActionId::DismissUpdateBanner => "dismiss-update-banner",
        ActionId::PreviewScrollUp => "preview-scroll-up",
        ActionId::PreviewScrollDown => "preview-scroll-down",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            view_mode: ViewMode::Structured,
            sort_order: SortOrder::Newest,
            has_search: false,
            project_group_selected: false,
            preview_scroll: false,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn parse_chord_reads_the_shared_tmux_grammar() {
        let chord = |code, ctrl, alt, shift| {
            Some(Chord {
                code,
                ctrl,
                alt,
                shift,
            })
        };
        let k = KeyCode::Char;
        let cases = [
            ("Ctrl+K", chord(k('k'), true, false, false)),
            ("C-k", chord(k('k'), true, false, false)),
            ("Shift+D", chord(k('D'), false, false, false)),
            ("Ctrl+Shift+G", chord(k('G'), true, false, false)),
            ("q", chord(k('q'), false, false, false)),
            ("Q", chord(k('q'), false, false, false)),
            ("F5", chord(KeyCode::F(5), false, false, false)),
            ("M-Up", alt_code(KeyCode::Up).into()),
            ("Alt+Down", alt_code(KeyCode::Down).into()),
            ("S-PageUp", shift_code(KeyCode::PageUp).into()),
            ("Shift+PgDn", shift_code(KeyCode::PageDown).into()),
            ("C-Up", ctrl_code(KeyCode::Up).into()),
            ("Ctrl+Alt+K", chord(k('k'), true, true, false)),
            ("", None),
            ("Ctrl+", None),
            ("Ctrl+K+J", None),
            ("Super+K", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_chord(input), want, "{input:?}");
        }
        // Declared chords round-trip through the grammar.
        for b in BINDINGS {
            for c in b.non_strict.iter().chain(b.strict) {
                if matches!(c.code, KeyCode::Up | KeyCode::PageUp | KeyCode::PageDown) {
                    let spec = format!(
                        "{}{}{}{:?}",
                        if c.ctrl { "C-" } else { "" },
                        if c.alt { "M-" } else { "" },
                        if c.shift { "S-" } else { "" },
                        c.code
                    );
                    assert_eq!(parse_chord(&spec).as_ref(), Some(c), "{spec}");
                }
            }
        }
        // Literal separator keys and spaced tokens still declare and match.
        for (spec, event) in [
            ("-", key('-')),
            ("Ctrl+-", ctrl_key('-')),
            ("Ctrl + J", ctrl_key('j')),
        ] {
            assert!(parse_chord(spec).is_some(), "{spec:?}");
            assert!(keybind_matches(spec, &event), "{spec:?}");
        }
        // A modified chord is not claimed by the bare-letter binding it resembles.
        assert!(!core_shadows(&parse_chord("M-n").unwrap()));
        assert!(core_shadows(&parse_chord("M-Up").unwrap()));
        assert!(!core_shadows(&parse_chord("C-x").unwrap()));
        // Shift narrows a plugin chord only against a core chord that requires it.
        assert!(core_shadows(&parse_chord("C-S-Up").unwrap()));
        assert!(!core_shadows(&parse_chord("PageUp").unwrap()));
        assert!(core_shadows(&parse_chord("S-PageUp").unwrap()));
    }

    #[test]
    fn core_chords_shadow_and_resolve_as_core_actions() {
        // `q` is the Quit binding; an unbound chord is neither shadowed nor resolved. With
        // no active plugins in the test process the merged resolver wraps core actions.
        assert!(core_shadows(&parse_chord("q").unwrap()));
        assert!(!core_shadows(&Chord {
            code: KeyCode::Char('z'),
            ctrl: true,
            alt: false,
            shift: false,
        }));
        let c = ctx();
        assert_eq!(
            resolve_action(&key('q'), false, &c),
            Some(ResolvedAction::Core(ActionId::Quit))
        );
        assert_eq!(resolve_action(&ctrl_key('z'), false, &c), None);
    }

    #[test]
    fn plugin_action_canonical_is_namespaced() {
        let a = PluginAction {
            plugin_id: "acme.kit".to_string(),
            action: "do-thing".to_string(),
        };
        assert_eq!(a.canonical(), "plugin.acme.kit.do-thing");

        // Idempotent when the manifest already targets a canonical command.
        let already = PluginAction {
            plugin_id: "acme.kit".to_string(),
            action: "plugin.acme.kit.do-thing".to_string(),
        };
        assert_eq!(already.canonical(), "plugin.acme.kit.do-thing");
    }

    /// Non-strict binds bare letters; strict moves the primary action to Shift+letter and
    /// the secondary to Ctrl+letter. Unread stays on Shift+U and sort on Ctrl+O in both.
    #[test]
    fn chord_resolution_by_mode() {
        let c = ctx();
        let non_strict = [
            (key('d'), ActionId::Delete),
            (key('D'), ActionId::Diff),
            (key('r'), ActionId::Rename),
            (key('R'), ActionId::Serve),
            (key('t'), ActionId::ToggleView),
            (key('T'), ActionId::AttachTerminal),
            (key('n'), ActionId::NewSession),
            (key('N'), ActionId::NewFromSelection),
            (key('p'), ActionId::Projects),
            (key('P'), ActionId::Profiles),
            (key('o'), ActionId::SortPicker),
            (key('g'), ActionId::GroupBy),
            (key('q'), ActionId::Quit),
            // `u` is Update regardless of whether an update is available.
            (key('u'), ActionId::Update),
            (key('U'), ActionId::ToggleUnread),
            (key('y'), ActionId::ToggleHideStopped),
            (ctrl_key('o'), ActionId::SortPicker),
        ];
        let strict = [
            (key('D'), ActionId::Delete),
            (key('R'), ActionId::Rename),
            (key('T'), ActionId::ToggleView),
            (key('N'), ActionId::NewSession),
            (key('P'), ActionId::Projects),
            (key('O'), ActionId::SortPicker),
            (key('U'), ActionId::ToggleUnread),
            (key('Y'), ActionId::ToggleHideStopped),
            (ctrl_key('d'), ActionId::Diff),
            (ctrl_key('r'), ActionId::Serve),
            (ctrl_key('t'), ActionId::AttachTerminal),
            (ctrl_key('n'), ActionId::NewFromSelection),
            (ctrl_key('p'), ActionId::Profiles),
            (ctrl_key('g'), ActionId::GroupBy),
            (ctrl_key('u'), ActionId::Update),
            (ctrl_key('o'), ActionId::SortPicker),
        ];
        for (strict_mode, cases) in [(false, &non_strict[..]), (true, &strict[..])] {
            for (event, want) in cases {
                assert_eq!(
                    resolve(event, strict_mode, &c),
                    Some(*want),
                    "strict={strict_mode} {event:?}"
                );
            }
        }
    }

    #[test]
    fn strict_bare_lowercase_action_letters_are_unbound() {
        // They fall through to the dispatcher's typing-guard, not an action.
        let c = ctx();
        for ch in [
            'd', 'r', 't', 'n', 'p', 's', 'x', 'm', 'e', 'i', 'z', 'g', 'o', 'u',
        ] {
            assert_eq!(resolve(&key(ch), true, &c), None, "strict bare '{ch}'");
        }
    }

    // #3038: a committed search must never shadow Shift+N. Only bare `n` cycles; every
    // `N` chord stays a new-session action.
    #[test]
    fn committed_search_cycles_n_but_never_shadows_shift_new_session() {
        let mut c = ctx();
        c.has_search = true;
        // Bare `n` cycles forward through matches in both modes.
        assert_eq!(resolve(&key('n'), false, &c), Some(ActionId::SearchNext));
        assert_eq!(resolve(&key('n'), true, &c), Some(ActionId::SearchNext));
        // Shift+N stays a new-session action even while a search is live:
        // NewFromSelection in non-strict, NewSession in strict.
        assert_eq!(
            resolve(&key('N'), false, &c),
            Some(ActionId::NewFromSelection)
        );
        assert_eq!(resolve(&key('N'), true, &c), Some(ActionId::NewSession));

        // Without a committed search, the same keys keep their new-session
        // meaning; `n` is only borrowed for cycling while matches exist.
        c.has_search = false;
        assert_eq!(resolve(&key('n'), false, &c), Some(ActionId::NewSession));
        assert_eq!(
            resolve(&key('N'), false, &c),
            Some(ActionId::NewFromSelection)
        );
        assert_eq!(resolve(&key('N'), true, &c), Some(ActionId::NewSession));
        assert_eq!(
            resolve(&ctrl_key('n'), true, &c),
            Some(ActionId::NewFromSelection)
        );
    }

    #[test]
    #[serial_test::serial]
    fn context_guards_gate_attention_and_terminal_actions() {
        // Snooze resolves only in the Attention sort. Favorite resolves there regardless
        // of `session.favorites_first`, which only opens it outside Attention.
        let original = crate::session::favorites_first();
        crate::session::set_favorites_first(false);

        let mut c = ctx();
        assert_eq!(resolve(&key('f'), false, &c), None);
        assert_eq!(resolve(&key('h'), false, &c), None);
        crate::session::set_favorites_first(true);
        assert_eq!(
            resolve(&key('f'), false, &c),
            Some(ActionId::ToggleFavorite),
            "favorites-first on: 'f' must work outside the Attention sort"
        );
        crate::session::set_favorites_first(false);
        c.sort_order = SortOrder::Attention;
        assert_eq!(
            resolve(&key('f'), false, &c),
            Some(ActionId::ToggleFavorite)
        );
        assert_eq!(resolve(&key('h'), false, &c), Some(ActionId::ToggleSnooze));

        crate::session::set_favorites_first(original);

        // Container toggle only resolves in Terminal view.
        let mut c = ctx();
        assert_eq!(resolve(&key('c'), false, &c), None);
        c.view_mode = ViewMode::Terminal;
        assert_eq!(
            resolve(&key('c'), false, &c),
            Some(ActionId::ToggleContainer)
        );
    }

    #[test]
    fn system_health_actions_are_palette_only() {
        let c = ctx();
        let f9 = KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE);
        assert_ne!(resolve(&f9, false, &c), Some(ActionId::ToggleDiagnostics));
        assert_eq!(
            palette_id(ActionId::ToggleDiagnostics),
            "toggle-diagnostics"
        );
        assert_eq!(palette_id(ActionId::OpenSystemHealth), "open-system-health");
        for action in [ActionId::ToggleDiagnostics, ActionId::OpenSystemHealth] {
            let binding = BINDINGS.iter().find(|b| b.id == action).unwrap();
            assert!(binding.non_strict.is_empty() && binding.strict.is_empty());
            assert!(binding.help.is_none());
            assert!(binding.palette.is_some());
        }
    }

    #[test]
    fn labels_match_mode() {
        assert_eq!(label(ActionId::Diff, false), "D");
        assert_eq!(label(ActionId::Diff, true), "Ctrl+D");
        assert_eq!(label(ActionId::Delete, true), "D");
        assert_eq!(label(ActionId::Projects, false), "p");
        assert_eq!(label(ActionId::Projects, true), "P");
        assert_eq!(label(ActionId::Profiles, true), "Ctrl+P");
        assert_eq!(label(ActionId::Restart, false), "e");
        // NextWaiting has no strict binding.
        assert_eq!(label(ActionId::NextWaiting, true), "");
    }

    /// The `?` overlay renders one row per help-listed binding and never
    /// consults `Context`, so two bindings sharing a chord show the same key
    /// twice (#3133: `p` for the pin toggle and `p` for the projects dialog).
    /// The convention that keeps that readable is that the guarded one states
    /// when it applies. Enforced here rather than by asserting a desc string,
    /// so it also catches a future guarded binding added onto a shared chord.
    #[test]
    fn shared_help_chords_document_their_guard() {
        // A qualifier the desc can use to say when the guarded action fires.
        let qualifies = |d: &str| ["only", "when ", "selected"].iter().any(|q| d.contains(q));
        for strict in [false, true] {
            let rows: Vec<(String, Context, &str)> = BINDINGS
                .iter()
                .filter_map(|b| {
                    let help = b.help.as_ref()?;
                    let l = label(b.id, strict);
                    (!l.is_empty()).then_some((l, b.context, help.desc))
                })
                .collect();
            for (l, context, desc) in &rows {
                let shared = rows.iter().filter(|(other, ..)| other == l).count() > 1;
                if shared && *context != Context::Always {
                    assert!(
                        qualifies(desc),
                        "strict={strict}: {l:?} is shared, so the {context:?} row \
                         must say when it applies, got {desc:?}"
                    );
                }
            }
        }
    }

    /// Modified chords spell out their key: without this the arrow bindings reach help and
    /// the palette with an empty label, which is how they were invisible when first added.
    #[test]
    fn modified_chords_render_a_visible_key_label() {
        for (id, expected) in [
            (ActionId::MoveRowUp, "Ctrl+Up"),
            (ActionId::MoveRowDown, "Ctrl+Down"),
            (ActionId::JumpPrevFinished, "Alt+Up"),
            (ActionId::JumpNextFinished, "Alt+Down"),
        ] {
            for strict in [false, true] {
                assert_eq!(label(id, strict), expected, "{id:?} strict={strict}");
            }
        }
        assert_eq!(label(ActionId::ToggleFavorite, false), "f");
    }

    #[test]
    fn app_and_live_chords_resolve_in_their_own_context() {
        let ev = |code, modifiers| KeyEvent::new(code, modifiers);
        let live = Ctx {
            preview_scroll: true,
            ..ctx()
        };
        let cases = [
            // (event, resolves in the list, resolves in live-send)
            (
                ev(KeyCode::Char('k'), KeyModifiers::CONTROL),
                Some(ActionId::OpenCommandPalette),
                Some(ActionId::OpenCommandPalette),
            ),
            (
                ev(KeyCode::Char('K'), KeyModifiers::CONTROL),
                Some(ActionId::OpenCommandPalette),
                Some(ActionId::OpenCommandPalette),
            ),
            (
                ev(KeyCode::PageUp, KeyModifiers::SHIFT),
                None,
                Some(ActionId::PreviewScrollUp),
            ),
            (
                ev(KeyCode::PageDown, KeyModifiers::SHIFT),
                None,
                Some(ActionId::PreviewScrollDown),
            ),
            (ev(KeyCode::PageUp, KeyModifiers::NONE), None, None),
            (
                ev(KeyCode::PageUp, KeyModifiers::SHIFT | KeyModifiers::CONTROL),
                None,
                None,
            ),
            (
                ev(KeyCode::Up, KeyModifiers::ALT),
                Some(ActionId::JumpPrevFinished),
                Some(ActionId::JumpPrevFinished),
            ),
        ];
        for strict in [false, true] {
            for (key, in_list, in_live) in &cases {
                assert_eq!(resolve(key, strict, &ctx()), *in_list, "{key:?} list");
                assert_eq!(resolve(key, strict, &live), *in_live, "{key:?} live");
            }
        }
        // App-owned: never resolves in Home (so plugins keep Ctrl+X), yet App still matches it.
        for c in [ctx(), live] {
            assert_eq!(resolve_action(&ctrl_key('x'), false, &c), None);
        }
        assert!(is_bound(
            ActionId::DismissUpdateBanner,
            false,
            &ctrl_key('x')
        ));
        assert!(!is_bound(ActionId::DismissUpdateBanner, false, &key('x')));
        assert_eq!(label(ActionId::PreviewScrollUp, false), "Shift+PgUp");
    }

    #[test]
    fn action_names_are_unique_kebab_case() {
        let mut seen = std::collections::HashSet::new();
        for b in BINDINGS {
            let name = palette_id(b.id);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name:?} is not kebab-case"
            );
            assert!(seen.insert(name), "{name:?} is used twice");
        }
    }
}
