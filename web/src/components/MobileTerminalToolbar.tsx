import { useCallback } from "react";
import type { RefObject } from "react";
import { useHoldRepeat } from "../hooks/useHoldRepeat";
import { readClipboardText } from "../lib/clipboard";
import { invalidateRetainedImeContext } from "../lib/mobileKeyboardProxy";
import { toolbarKeySpec, type ToolbarKeyId, type ToolbarKeySpec } from "../lib/terminalToolbarKeys";
import { StrokeIcon } from "./icons";

function execCommandPaste(): boolean {
  try {
    return document.execCommand("paste");
  } catch {
    return false;
  }
}

interface Props {
  /** The configured row, in order. */
  keys: readonly ToolbarKeyId[];
  sendData: (data: string) => boolean;
  sendPaste: (text: string, submit: boolean) => boolean;
  /** Opens the compose sheet, also the fallback when the clipboard cannot be read. */
  onCompose: () => void;
  keyboardOpen: boolean;
  /** No soft keyboard: sit lower, smaller, and inset from the rounded screen corners. */
  compact: boolean;
  ctrlActive: boolean;
  onCtrlToggle: () => void;
  /** The live view's hidden input element, which owns keyboard focus. */
  inputElRef: RefObject<HTMLTextAreaElement | null>;
}

// Uniform key caps: a framed surface reads as a key, and one height, label size, and icon weight keep the row calm.
const KEY_BASE =
  "flex-1 min-w-0 h-10 flex items-center justify-center rounded-md border shadow-[inset_0_-1px_0_rgb(0_0_0/0.3)] transition-colors duration-75 select-none touch-manipulation [-webkit-touch-callout:none]";
const KEY_CLASS = `${KEY_BASE} border-surface-700/70 bg-surface-800 text-text-primary active:bg-surface-700 active:border-surface-600`;
const LATCHED_KEY_CLASS = `${KEY_BASE} border-brand-500/80 bg-brand-600/30 text-brand-400`;
// Compose is the primary action: a neutral cap with an accent glyph, so it never reads as latched.
const COMPOSE_KEY_CLASS = `${KEY_BASE} border-surface-700/70 bg-surface-800 text-brand-400 active:bg-surface-700 active:border-surface-600`;

const ARROW_ROTATION = { up: 0, right: 90, down: 180, left: 270 } as const;

/** An icon where the mono font's glyph renders small or boxed, otherwise the label. */
function KeyFace({ spec }: { spec: ToolbarKeySpec }) {
  switch (spec.id) {
    case "backspace":
      return (
        <StrokeIcon size={18} strokeWidth="1.75" hidden>
          <path d="M10 5a2 2 0 0 0-1.344.519l-6.328 5.74a1 1 0 0 0 0 1.481l6.328 5.741A2 2 0 0 0 10 19h10a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2z" />
          <path d="m12 9 6 6" />
          <path d="m18 9-6 6" />
        </StrokeIcon>
      );
    case "enter":
      return (
        <StrokeIcon size={18} strokeWidth="1.75" hidden>
          <path d="M20 4v7a4 4 0 0 1-4 4H4" />
          <path d="m9 10-5 5 5 5" />
        </StrokeIcon>
      );
    case "up":
    case "down":
    case "left":
    case "right":
      return (
        <StrokeIcon size={18} strokeWidth="1.75" hidden>
          <g transform={`rotate(${ARROW_ROTATION[spec.id]} 12 12)`}>
            <path d="M12 19V5" />
            <path d="m5 12 7-7 7 7" />
          </g>
        </StrokeIcon>
      );
    default:
      return <span className="font-mono text-[12px] font-medium tracking-tight">{spec.label}</span>;
  }
}

function RepeatKey({ spec, onSend }: { spec: ToolbarKeySpec; onSend: (data: string) => void }) {
  const handlers = useHoldRepeat(() => onSend(spec.data!));
  return (
    <button type="button" aria-label={spec.name} className={KEY_CLASS} {...handlers}>
      <KeyFace spec={spec} />
    </button>
  );
}

/** One row of the user's configured terminal keys above the soft keyboard. */
export function MobileTerminalToolbar({
  keys,
  sendData,
  sendPaste,
  onCompose,
  keyboardOpen,
  compact,
  ctrlActive,
  onCtrlToggle,
  inputElRef,
}: Props) {
  const haptic = useCallback(() => {
    navigator.vibrate?.(10);
  }, []);

  const refocusTerminal = useCallback(() => {
    // Only re-focus if the input already had focus (keyboard open);
    // a toolbar tap must not summon the keyboard on its own.
    if (keyboardOpen) inputElRef.current?.focus();
  }, [inputElRef, keyboardOpen]);

  // Every toolbar key reaches the PTY without a `beforeinput` on either hidden input, so the retained IME syllable
  // stops mirroring the line it shadowed.
  const send = useCallback(
    (data: string) => {
      haptic();
      invalidateRetainedImeContext(inputElRef.current);
      sendData(data);
      refocusTerminal();
    },
    [sendData, inputElRef, refocusTerminal, haptic],
  );

  const paste = async () => {
    haptic();
    if (!window.isSecureContext) {
      // No Clipboard API on a plain-HTTP origin.
      const active = document.activeElement;
      const editable = active instanceof HTMLTextAreaElement || active instanceof HTMLInputElement;
      if (keyboardOpen && editable && execCommandPaste()) return;
      onCompose();
      return;
    }
    const text = await readClipboardText();
    if (!text) {
      // The compose sheet's native long-press paste still works.
      onCompose();
      return;
    }
    invalidateRetainedImeContext(inputElRef.current);
    sendPaste(text, false);
  };

  const renderKey = (spec: ToolbarKeySpec) => {
    switch (spec.id) {
      case "ctrl":
        return (
          <button
            key={spec.id}
            type="button"
            aria-label={spec.name}
            aria-pressed={ctrlActive}
            className={ctrlActive ? LATCHED_KEY_CLASS : KEY_CLASS}
            onClick={() => {
              haptic();
              onCtrlToggle();
            }}
          >
            <KeyFace spec={spec} />
          </button>
        );
      case "paste":
        return (
          <button key={spec.id} type="button" aria-label={spec.name} className={KEY_CLASS} onClick={paste}>
            <StrokeIcon size={18} strokeWidth="1.75" hidden>
              <rect x="9" y="2" width="6" height="4" rx="1" />
              <path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2" />
            </StrokeIcon>
          </button>
        );
      case "compose":
        return (
          <button
            key={spec.id}
            type="button"
            aria-label={spec.name}
            className={COMPOSE_KEY_CLASS}
            onClick={() => {
              haptic();
              onCompose();
            }}
          >
            <StrokeIcon size={18} strokeWidth="1.75" hidden>
              <path d="M12 20h9" />
              <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
            </StrokeIcon>
          </button>
        );
      case "ctrl-c":
        return (
          <button
            key={spec.id}
            type="button"
            aria-label={spec.name}
            className={KEY_CLASS}
            onClick={() => {
              send(spec.data!);
              if (ctrlActive) onCtrlToggle();
            }}
          >
            <KeyFace spec={spec} />
          </button>
        );
      default:
        return spec.repeat ? (
          <RepeatKey key={spec.id} spec={spec} onSend={send} />
        ) : (
          <button
            key={spec.id}
            type="button"
            aria-label={spec.name}
            className={KEY_CLASS}
            onClick={() => send(spec.data!)}
          >
            <KeyFace spec={spec} />
          </button>
        );
    }
  };

  if (keys.length === 0) return null;
  return (
    <div
      // The parent drops its home-indicator padding for this bar (index.css .home-indicator-clearance), so the bar
      // runs to the screen edge and owns the clearance. With the keyboard up iOS may still report the inset, so the
      // keys keep all of it. Without, they drop to 22px less than the inset and move in from the rounded corners.
      data-terminal-toolbar
      data-compact={compact || undefined}
      className={`shrink-0 flex items-center gap-1.5 pt-1.5 bg-surface-900 border-t border-surface-700/50 ${
        compact
          ? "px-[max(0.5rem,calc(env(safe-area-inset-bottom)*0.7))] pb-[max(0.375rem,calc(env(safe-area-inset-bottom)-1.375rem))] [&_button]:h-9 [&_svg]:size-4 [&_span]:text-[11px]"
          : "px-2 pb-[calc(env(safe-area-inset-bottom)+0.375rem)]"
      }`}
      // Prevent toolbar taps from stealing focus away from the proxy input.
      onMouseDown={(e) => e.preventDefault()}
    >
      {keys.map((id) => renderKey(toolbarKeySpec(id)))}
    </div>
  );
}
