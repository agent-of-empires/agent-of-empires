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
    Diff,
    Serve,
    Settings,
    Profiles,
    Projects,
    Restart,
    Update,
    ToggleArchive,
    ToggleFavorite,
    ToggleSnooze,
    /// Toggle the selected session's unread marker (read -> manual-unread;
    /// unread -> read). Gated behind the `session.unread_indicator` config
    /// toggle (on by default); a no-op when disabled.
    ToggleUnread,
    ToggleContainer,
    TogglePreviewInfo,
    SortPicker,
    GroupBy,
    NextWaiting,
    /// Open the plugin manager (palette only; no default chord).
    Plugins,
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
}

/// A single chord. `ctrl` requires the Control modifier; Shift is implicit in
/// the uppercase letter `code` (terminals deliver `Shift+d` as `Char('D')`,
/// and iOS Mosh delivers a bare uppercase keycode with no Shift modifier, so
/// matching on the uppercase code rather than a Shift flag covers both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
}

const fn k(c: char) -> Chord {
    Chord {
        code: KeyCode::Char(c),
        ctrl: false,
    }
}

const fn ctrl(c: char) -> Chord {
    Chord {
        code: KeyCode::Char(c),
        ctrl: true,
    }
}

const fn f(n: u8) -> Chord {
    Chord {
        code: KeyCode::F(n),
        ctrl: false,
    }
}

/// Contextual guard: a binding only resolves when its context holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Always,
    TerminalView,
    AttentionSort,
    SearchActive,
    /// The cursor is on a real (non-synthetic) project header in project view.
    ProjectGroupSelected,
    /// The unread-session feature is enabled (`session.unread_indicator`). When
    /// off, the binding is removed from dispatch so the key isn't swallowed by
    /// a dead action; help and the command palette skip it separately.
    UnreadEnabled,
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
    /// Only included when the `serve` feature is on (just the Serve command).
    pub serve_only: bool,
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
}

fn chord_matches(c: &Chord, key: &KeyEvent) -> bool {
    // Match the Ctrl modifier exactly: a non-ctrl chord must NOT fire when
    // Ctrl is held. Otherwise `k('q')` would also match Ctrl+Q (reserved for
    // exiting live-send mode, #1569) and `k('d')` would match Ctrl+D, letting
    // a modified chord trigger a bare-letter action.
    key.code == c.code && key.modifiers.contains(KeyModifiers::CONTROL) == c.ctrl
}

fn context_holds(context: Context, ctx: &Ctx) -> bool {
    match context {
        Context::Always => true,
        Context::TerminalView => ctx.view_mode == ViewMode::Terminal,
        Context::AttentionSort => ctx.sort_order == SortOrder::Attention,
        Context::SearchActive => ctx.has_search,
        Context::ProjectGroupSelected => ctx.project_group_selected,
        Context::UnreadEnabled => crate::session::unread_enabled(),
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
/// Only the structured view (serve-gated) executes plugin commands, so this is
/// unused in a bare-core build; gate it to avoid a dead-code warning there.
#[cfg(feature = "serve")]
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

/// Parse a key-chord string like `Ctrl+K`, `Shift+D`, `F5`, or `q` into a
/// [`Chord`]. Supports `Ctrl`/`Shift` modifiers, single characters, and
/// function keys. Returns `None` for anything else.
pub fn parse_chord(s: &str) -> Option<Chord> {
    let mut ctrl = false;
    let mut shift = false;
    let mut key: Option<&str> = None;
    for tok in s.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            // Unsupported modifiers and a second key token are rejected rather
            // than silently remapped: `Alt+K` must not collapse to a bare `k`
            // that hijacks core navigation.
            "alt" | "option" | "meta" | "super" | "cmd" => return None,
            _ if key.is_none() => key = Some(tok),
            _ => return None,
        }
    }
    let key = key?;
    let code = if key.len() == 1 {
        // Match the table's convention: bare letters are lowercase chars, Shift
        // is encoded as the uppercase char (terminals deliver Ctrl+k as a
        // lowercase Char with the CONTROL modifier, Shift+d as Char('D')).
        let c = key.chars().next().unwrap();
        let c = if shift {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        };
        KeyCode::Char(c)
    } else {
        let n = key
            .strip_prefix(['F', 'f'])
            .and_then(|n| n.parse::<u8>().ok())?;
        KeyCode::F(n)
    };
    Some(Chord { code, ctrl })
}

/// Whether a core binding already claims `chord` in either mode. Used by
/// `aoe plugin info` to flag a plugin keybind that core shadows.
pub fn core_shadows(chord: &Chord) -> bool {
    BINDINGS.iter().any(|b| {
        b.non_strict
            .iter()
            .chain(b.strict)
            .any(|c| c.code == chord.code && c.ctrl == chord.ctrl)
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

fn format_chord(c: &Chord) -> String {
    match c.code {
        KeyCode::Char(ch) if c.ctrl => format!("Ctrl+{}", ch.to_ascii_uppercase()),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => String::new(),
    }
}

// Order matters: context-guarded bindings that share a chord with an unguarded
// one (search-cycle vs new, etc.) come first so they win when their guard holds.
pub static BINDINGS: &[Binding] = &[
    // --- search cycle (only while matches are active; both modes) ---
    // Only bare `n` cycles (forward, wrapping). `N`/Shift+N stays a new-session
    // key in every state so a committed search never shadows it (#3038); the
    // forward wrap keeps every match reachable, so there is no reverse binding.
    Binding {
        id: ActionId::SearchNext,
        non_strict: &[k('n')],
        strict: &[k('n')],
        context: Context::SearchActive,
        help: None,
        palette: None,
    },
    // --- attention-sort triage ---
    Binding {
        id: ActionId::ToggleFavorite,
        non_strict: &[k('f')],
        strict: &[k('F')],
        context: Context::AttentionSort,
        help: Some(HelpMeta {
            section: HelpSection::Attention,
            desc: "Toggle favorite (Attention sort)",
        }),
        palette: Some(PaletteMeta {
            title: "Toggle favorite",
            keywords: &["star", "pin", "fav"],
            group: PaletteGroup::Actions,
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
        }),
    },
    Binding {
        id: ActionId::Serve,
        non_strict: &[k('R')],
        strict: &[ctrl('r')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Other,
            desc: "Serve (LAN / Tunnel)",
        }),
        palette: Some(PaletteMeta {
            title: "Open serve (LAN / Tunnel)",
            keywords: &["web", "remote", "phone", "tunnel"],
            group: PaletteGroup::Settings,
            serve_only: true,
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
            serve_only: false,
        }),
    },
    // Pin toggle shares `p` (Shift+P in strict) with Projects, but only fires
    // when a project header is selected, so it must precede the Projects
    // binding. On a project header `p` pins/unpins; everywhere else `p` still
    // opens the projects dialog.
    Binding {
        id: ActionId::ToggleProjectPin,
        non_strict: &[k('p')],
        strict: &[k('P')],
        context: Context::ProjectGroupSelected,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Pin/unpin project (keep without sessions)",
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
        }),
    },
    // `U` toggles read/unread, pinned to Shift+u in BOTH modes (matches the
    // macOS Mail "mark unread" muscle memory and keeps the key stable). It does
    // NOT participate in the strict relocation: `U` is already a modified key,
    // so it satisfies strict mode's "no bare action letters" rule as-is.
    // `u` updates (when available) and relocates the usual way: bare `u` in
    // non-strict, `Ctrl+u` in strict.
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
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
            serve_only: false,
        }),
    },
    // Tips overlay. No key chords: it's reached from the palette, the badge,
    // and the `?` help screen, so it never shadows a typing-guard key. `help`
    // is None because the help overlay skips keyless rows; it gets a bespoke
    // row in `components/help.rs` instead.
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
            serve_only: false,
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
            serve_only: false,
        }),
    },
    // Palette-only: Shift+F collides with strict-mode ToggleFavorite under
    // Attention sort and the home keyspace is saturated, so fork is reached
    // from the command palette and the context menu only.
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
            serve_only: false,
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
        ActionId::Diff => "diff",
        ActionId::Serve => "serve",
        ActionId::Settings => "settings",
        ActionId::Profiles => "profiles",
        ActionId::Projects => "projects",
        ActionId::Restart => "restart",
        ActionId::ToggleArchive => "archive",
        ActionId::ToggleFavorite => "favorite",
        ActionId::ToggleSnooze => "snooze",
        ActionId::ToggleUnread => "toggle-unread",
        ActionId::TogglePreviewInfo => "toggle-preview-info",
        ActionId::SortPicker => "pick-sort",
        ActionId::GroupBy => "pick-group-by",
        ActionId::Help => "help",
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
        ActionId::Fork => "fork",
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
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn parse_chord_handles_modifiers_and_keys() {
        assert_eq!(
            parse_chord("Ctrl+K"),
            Some(Chord {
                code: KeyCode::Char('k'),
                ctrl: true
            })
        );
        assert_eq!(
            parse_chord("Shift+D"),
            Some(Chord {
                code: KeyCode::Char('D'),
                ctrl: false
            })
        );
        assert_eq!(
            parse_chord("q"),
            Some(Chord {
                code: KeyCode::Char('q'),
                ctrl: false
            })
        );
        assert_eq!(
            parse_chord("F5"),
            Some(Chord {
                code: KeyCode::F(5),
                ctrl: false
            })
        );
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("Ctrl+"), None);
    }

    #[test]
    fn parse_chord_rejects_unsupported_and_repeated_tokens() {
        // Unknown modifiers must not collapse to a bare key that hijacks core
        // navigation, and a chord may carry at most one key token.
        assert_eq!(parse_chord("Alt+K"), None);
        assert_eq!(parse_chord("Ctrl+Alt+K"), None);
        assert_eq!(parse_chord("Ctrl+K+J"), None);
        assert_eq!(parse_chord("Meta+K"), None);
    }

    #[test]
    fn core_shadows_known_core_chords() {
        // `q` is the Quit binding; an unbound chord is not shadowed.
        assert!(core_shadows(&parse_chord("q").unwrap()));
        assert!(!core_shadows(&Chord {
            code: KeyCode::Char('z'),
            ctrl: true
        }));
    }

    #[test]
    fn resolve_action_wraps_core_bindings() {
        // With no active plugins in the test process, the merged resolver just
        // returns the core action, wrapped as Core.
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

    #[test]
    fn non_strict_resolution() {
        let c = ctx();
        let cases = [
            ('d', ActionId::Delete),
            ('D', ActionId::Diff),
            ('r', ActionId::Rename),
            ('R', ActionId::Serve),
            ('t', ActionId::ToggleView),
            ('T', ActionId::AttachTerminal),
            ('n', ActionId::NewSession),
            ('N', ActionId::NewFromSelection),
            ('p', ActionId::Projects),
            ('P', ActionId::Profiles),
            ('o', ActionId::SortPicker),
            ('g', ActionId::GroupBy),
            ('q', ActionId::Quit),
            // `u` is Update (unread lives on Shift+U); resolves regardless of
            // whether an update is actually available.
            ('u', ActionId::Update),
        ];
        for (ch, want) in cases {
            assert_eq!(
                resolve(&key(ch), false, &c),
                Some(want),
                "non-strict '{ch}'"
            );
        }
    }

    #[test]
    fn shift_u_toggles_unread_in_both_modes_and_u_updates() {
        let c = ctx();
        // Unread is pinned to Shift+U regardless of strict mode.
        assert_eq!(
            resolve(&key('U'), false, &c),
            Some(ActionId::ToggleUnread),
            "non-strict U = unread"
        );
        assert_eq!(
            resolve(&key('U'), true, &c),
            Some(ActionId::ToggleUnread),
            "strict U = unread"
        );
        // Update relocates the usual way: bare `u` in non-strict, `Ctrl+u`
        // in strict (and never collides with unread on `U`).
        assert_eq!(
            resolve(&key('u'), false, &c),
            Some(ActionId::Update),
            "non-strict u = update"
        );
        assert_eq!(
            resolve(&ctrl_key('u'), true, &c),
            Some(ActionId::Update),
            "strict Ctrl+u = update"
        );
    }

    #[test]
    fn strict_relocation_is_consistent() {
        let c = ctx();
        // Shift+letter (the uppercase code) drives the primary action; the
        // secondary action moves to Ctrl+letter. No bare lowercase action keys.
        let shifted = [
            ('D', ActionId::Delete),
            ('R', ActionId::Rename),
            ('T', ActionId::ToggleView),
            ('N', ActionId::NewSession),
            ('P', ActionId::Projects),
            ('O', ActionId::SortPicker),
            ('U', ActionId::ToggleUnread),
        ];
        for (ch, want) in shifted {
            assert_eq!(resolve(&key(ch), true, &c), Some(want), "strict '{ch}'");
        }
        let ctrled = [
            ('d', ActionId::Diff),
            ('r', ActionId::Serve),
            ('t', ActionId::AttachTerminal),
            ('n', ActionId::NewFromSelection),
            ('p', ActionId::Profiles),
            ('g', ActionId::GroupBy),
            ('u', ActionId::Update),
        ];
        for (ch, want) in ctrled {
            assert_eq!(
                resolve(&ctrl_key(ch), true, &c),
                Some(want),
                "strict Ctrl+{ch}"
            );
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

    // #3038: a committed search must never shadow Shift+N. Only bare `n` cycles
    // (forward); every `N`/Shift+N chord stays a new-session action whether or
    // not a search is committed.
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
    fn context_guards_gate_attention_and_terminal_actions() {
        // Favorite/snooze only resolve in Attention sort.
        let mut c = ctx();
        assert_eq!(resolve(&key('f'), false, &c), None);
        c.sort_order = SortOrder::Attention;
        assert_eq!(
            resolve(&key('f'), false, &c),
            Some(ActionId::ToggleFavorite)
        );
        assert_eq!(resolve(&key('h'), false, &c), Some(ActionId::ToggleSnooze));

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
    fn ctrl_o_sorts_in_both_modes() {
        let c = ctx();
        assert_eq!(
            resolve(&ctrl_key('o'), false, &c),
            Some(ActionId::SortPicker)
        );
        assert_eq!(
            resolve(&ctrl_key('o'), true, &c),
            Some(ActionId::SortPicker)
        );
    }

    #[test]
    fn fork_is_palette_only_no_chord() {
        let c = ctx();
        // No chord resolves to Fork in either mode (palette-only, like Plugins).
        for ch in ['f', 'F'] {
            assert_ne!(resolve(&key(ch), false, &c), Some(ActionId::Fork));
            assert_ne!(resolve(&key(ch), true, &c), Some(ActionId::Fork));
        }
        // Fork has a stable palette id.
        assert_eq!(palette_id(ActionId::Fork), "fork");
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
}
