import { IS_MAC, SHORTCUTS, SHORTCUTS_BY_ID, formatHelpShortcut } from "../lib/shortcuts";
import { CAPTION } from "../lib/surfaces";
import { CloseButton, Dialog } from "./Dialog";

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
  { key: "One finger", desc: "Swipe up/down to scroll the terminal" },
  { key: "Two fingers", desc: "Pinch to change the font size" },
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
    <Dialog
      id="help-overlay"
      title={<span className="text-text-bright">Help</span>}
      describedBy={false}
      panelClassName="md:w-[480px] md:max-h-[90vh]"
      bodyClassName="space-y-5"
      onDismiss={onClose}
      headerEnd={<CloseButton onClick={onClose} />}
      footer={
        <p className="mr-auto text-[13px] md:text-sm text-text-dim">
          Single-key shortcuts are disabled when typing in inputs.{" "}
          {formatHelpShortcut(SHORTCUTS_BY_ID.palette.chord, IS_MAC)} works everywhere.
        </p>
      }
    >
      <ShortcutGroup title="Dashboard" rows={shortcuts} keyClassName="text-brand-500 min-w-[32px]" />
      <ShortcutGroup title="Terminal" rows={TERMINAL_SHORTCUTS} keyClassName="text-accent-600 min-w-[32px]" />
      <ShortcutGroup title="Mobile gestures" rows={MOBILE_GESTURES} keyClassName="text-accent-600 whitespace-nowrap" />
    </Dialog>
  );
}

function ShortcutGroup({
  title,
  rows,
  keyClassName,
}: {
  title: string;
  rows: { key: string; desc: string }[];
  keyClassName: string;
}) {
  return (
    <section>
      <h3 className={CAPTION}>{title}</h3>
      <div className="space-y-1.5">
        {rows.map((s) => (
          <div key={s.key} className="flex items-center gap-3">
            <kbd
              className={`font-mono text-[13px] md:text-xs bg-surface-900 border border-surface-700 rounded-md px-1.5 py-0.5 text-center ${keyClassName}`}
            >
              {s.key}
            </kbd>
            <span className="text-[15px] md:text-sm text-text-secondary">{s.desc}</span>
          </div>
        ))}
      </div>
    </section>
  );
}
