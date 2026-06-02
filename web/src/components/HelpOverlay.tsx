import {
  IS_MAC,
  SHORTCUTS,
  SHORTCUTS_BY_ID,
  formatHelpShortcut,
} from "../lib/shortcuts";

interface Props {
  onClose: () => void;
}

const TERMINAL_SHORTCUTS = [
  { key: "All keys", desc: "Relayed directly to the agent via PTY" },
  { key: "Ctrl+C", desc: "Send interrupt to agent" },
  { key: "Ctrl+D", desc: "Send EOF to agent" },
  { key: "Up/Down", desc: "Scroll terminal history" },
];

const MOBILE_GESTURES = [
  { key: "Two fingers", desc: "Swipe up/down to scroll the terminal (tmux copy-mode)" },
  { key: "Tap pane", desc: "Open the soft keyboard" },
  { key: "Long-press ↑↓", desc: "Drag horizontally to emit ← →" },
  { key: "Hold session", desc: "Long-press a session to rename it" },
];

export function HelpOverlay({ onClose }: Props) {
  const shortcuts = SHORTCUTS.map((s) => ({
    key: formatHelpShortcut(s.chord, IS_MAC),
    desc: s.description,
  }));

  return (
    <div
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onClose}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[480px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-5 py-4 border-b border-surface-700">
          <h2 className="text-sm font-semibold text-text-bright">Help</h2>
          <button
            onClick={onClose}
            className="text-text-muted hover:text-text-secondary cursor-pointer"
          >
            &times;
          </button>
        </div>

        <div className="p-5">
          <div className="mb-5">
            <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-2">
              Dashboard
            </h3>
            <div className="space-y-1">
              {shortcuts.map((s) => (
                <div key={s.key} className="flex items-center gap-3">
                  <kbd className="font-mono text-sm bg-surface-900 border border-surface-700 rounded px-1.5 py-0.5 text-brand-500 min-w-[32px] text-center">
                    {s.key}
                  </kbd>
                  <span className="text-sm text-text-secondary">
                    {s.desc}
                  </span>
                </div>
              ))}
            </div>
          </div>

          <div className="mb-5">
            <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-2">
              Terminal
            </h3>
            <div className="space-y-1">
              {TERMINAL_SHORTCUTS.map((s) => (
                <div key={s.key} className="flex items-center gap-3">
                  <kbd className="font-mono text-sm bg-surface-900 border border-surface-700 rounded px-1.5 py-0.5 text-accent-600 min-w-[32px] text-center">
                    {s.key}
                  </kbd>
                  <span className="text-sm text-text-secondary">
                    {s.desc}
                  </span>
                </div>
              ))}
            </div>
          </div>

          <div>
            <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-2">
              Mobile gestures
            </h3>
            <div className="space-y-1">
              {MOBILE_GESTURES.map((g) => (
                <div key={g.key} className="flex items-center gap-3">
                  <kbd className="font-mono text-xs bg-surface-900 border border-surface-700 rounded px-1.5 py-0.5 text-accent-600 text-center whitespace-nowrap">
                    {g.key}
                  </kbd>
                  <span className="text-sm text-text-secondary">{g.desc}</span>
                </div>
              ))}
            </div>
          </div>
        </div>

        <div className="px-5 py-3 border-t border-surface-700">
          <p className="text-sm text-text-dim">
            Single-key shortcuts are disabled when typing in inputs.{" "}
            {formatHelpShortcut(SHORTCUTS_BY_ID.palette.chord, IS_MAC)} works
            everywhere.
          </p>
        </div>
      </div>
    </div>
  );
}
