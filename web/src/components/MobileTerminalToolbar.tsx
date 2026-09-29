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
  ctrlActive: boolean;
  onCtrlToggle: () => void;
  /** The live view's hidden input element, which owns keyboard focus. */
  inputElRef: RefObject<HTMLTextAreaElement | null>;
}

const KEY_CLASS =
  "flex-1 min-w-0 flex items-center justify-center h-11 rounded-md transition-colors duration-75 text-text-secondary select-none touch-manipulation [-webkit-touch-callout:none] active:bg-surface-700/50";

function KeyLabel({ label }: { label: string }) {
  return <span className={`font-mono ${label.length > 2 ? "text-xs" : "text-sm"}`}>{label}</span>;
}

function RepeatKey({ spec, onSend }: { spec: ToolbarKeySpec; onSend: (data: string) => void }) {
  const handlers = useHoldRepeat(() => onSend(spec.data!));
  return (
    <button type="button" aria-label={spec.name} className={KEY_CLASS} {...handlers}>
      <KeyLabel label={spec.label} />
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
            className={
              ctrlActive ? `${KEY_CLASS.replace("text-text-secondary", "text-brand-400")} bg-brand-600/20` : KEY_CLASS
            }
            onClick={() => {
              haptic();
              onCtrlToggle();
            }}
          >
            <KeyLabel label={spec.label} />
          </button>
        );
      case "paste":
        return (
          <button key={spec.id} type="button" aria-label={spec.name} className={KEY_CLASS} onClick={paste}>
            <StrokeIcon size={14} strokeWidth="2" hidden>
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
            className={`${KEY_CLASS} text-brand-400`}
            onClick={() => {
              haptic();
              onCompose();
            }}
          >
            <StrokeIcon size={16} strokeWidth="2" hidden>
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
            <KeyLabel label={spec.label} />
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
            <KeyLabel label={spec.label} />
          </button>
        );
    }
  };

  if (keys.length === 0) return null;
  return (
    <div
      className="shrink-0 flex items-center gap-0.5 px-1 py-1.5 bg-surface-850 border-t border-surface-700/20"
      // Prevent toolbar taps from stealing focus away from the proxy input.
      onMouseDown={(e) => e.preventDefault()}
    >
      {keys.map((id) => renderKey(toolbarKeySpec(id)))}
    </div>
  );
}
