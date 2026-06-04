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
    SearchPrev,
    NewSession,
    NewFromSelection,
    NewFromProject,
    AttachTerminal,
    ToggleView,
    SendMessage,
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
    ToggleContainer,
    TogglePreviewInfo,
    SortPicker,
    GroupBy,
    NextWaiting,
}

/// A single chord. `ctrl` requires the Control modifier; Shift is implicit in
/// the uppercase letter `code` (terminals deliver `Shift+d` as `Char('D')`,
/// and iOS Mosh delivers a bare uppercase keycode with no Shift modifier, so
/// matching on the uppercase code rather than a Shift flag covers both).
#[derive(Debug, Clone, Copy)]
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
    Binding {
        id: ActionId::SearchNext,
        non_strict: &[k('n')],
        strict: &[k('n')],
        context: Context::SearchActive,
        help: None,
        palette: None,
    },
    Binding {
        id: ActionId::SearchPrev,
        non_strict: &[k('N')],
        strict: &[k('N')],
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
        id: ActionId::Stop,
        non_strict: &[k('x')],
        strict: &[k('X')],
        context: Context::Always,
        help: Some(HelpMeta {
            section: HelpSection::Actions,
            desc: "Stop session",
        }),
        palette: Some(PaletteMeta {
            title: "Stop session",
            keywords: &["kill", "end", "halt"],
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
    Binding {
        id: ActionId::Update,
        non_strict: &[k('u')],
        strict: &[k('u')],
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
        ActionId::TogglePreviewInfo => "toggle-preview-info",
        ActionId::SortPicker => "pick-sort",
        ActionId::GroupBy => "pick-group-by",
        ActionId::Help => "help",
        ActionId::NextWaiting => "next-waiting",
        ActionId::Quit => "quit",
        ActionId::ToolPicker => "tool-picker",
        ActionId::SearchStart => "search",
        ActionId::SearchNext => "search-next",
        ActionId::SearchPrev => "search-prev",
        ActionId::Update => "update",
        ActionId::ToggleContainer => "toggle-container",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            view_mode: ViewMode::Agent,
            sort_order: SortOrder::Newest,
            has_search: false,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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
            'd', 'r', 't', 'n', 'p', 's', 'x', 'm', 'e', 'i', 'z', 'g', 'o',
        ] {
            assert_eq!(resolve(&key(ch), true, &c), None, "strict bare '{ch}'");
        }
    }

    #[test]
    fn search_cycle_overrides_new_session_when_active() {
        let mut c = ctx();
        c.has_search = true;
        for strict in [false, true] {
            assert_eq!(resolve(&key('n'), strict, &c), Some(ActionId::SearchNext));
            assert_eq!(resolve(&key('N'), strict, &c), Some(ActionId::SearchPrev));
        }
        // Without an active search, the same keys are new-session actions.
        c.has_search = false;
        assert_eq!(resolve(&key('n'), false, &c), Some(ActionId::NewSession));
        assert_eq!(resolve(&key('N'), true, &c), Some(ActionId::NewSession));
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
