import { useCallback, useState } from "react";
import type { RefObject } from "react";
import { useLongPressDrag, type DragAxis } from "../hooks/useLongPressDrag";
import { bracketedPaste, readClipboardText } from "../lib/clipboard";
import { toastBus } from "../lib/toastBus";

function execCommandPaste(): boolean {
  try {
    return document.execCommand("paste");
  } catch {
    return false;
  }
}

interface Props {
  sendData: (data: string) => void;
  keyboardOpen: boolean;
  ctrlActive: boolean;
  onCtrlToggle: () => void;
  /** The live view's hidden input element, which owns keyboard focus. */
  inputElRef: RefObject<HTMLTextAreaElement | null>;
}

const ARROW_UP = "\x1b[A";
const ARROW_DOWN = "\x1b[B";
const ARROW_LEFT = "\x1b[D";
const ARROW_RIGHT = "\x1b[C";

export function MobileTerminalToolbar({ sendData, keyboardOpen, ctrlActive, onCtrlToggle, inputElRef }: Props) {
  const [upAxis, setUpAxis] = useState<DragAxis>("vertical");
  const [downAxis, setDownAxis] = useState<DragAxis>("vertical");

  const haptic = useCallback(() => {
    navigator.vibrate?.(10);
  }, []);

  const refocusTerminal = useCallback(() => {
    // Only re-focus if the input already had focus (keyboard open);
    // a toolbar tap must not summon the keyboard on its own.
    if (keyboardOpen) inputElRef.current?.focus();
  }, [inputElRef, keyboardOpen]);

  const send = useCallback(
    (data: string) => {
      haptic();
      sendData(data);
      refocusTerminal();
    },
    [sendData, refocusTerminal, haptic],
  );

  const upHandlers = useLongPressDrag({
    onRepeat: () => sendData(ARROW_UP),
    onHorizontal: (dir) => sendData(dir === "left" ? ARROW_LEFT : ARROW_RIGHT),
    onAxisChange: setUpAxis,
  });
  const downHandlers = useLongPressDrag({
    onRepeat: () => sendData(ARROW_DOWN),
    onHorizontal: (dir) => sendData(dir === "left" ? ARROW_LEFT : ARROW_RIGHT),
    onAxisChange: setDownAxis,
  });

  const btnBase =
    "flex-1 flex items-center justify-center h-11 rounded-md transition-colors duration-75 text-text-secondary select-none touch-manipulation relative active:bg-surface-700/50 active:scale-95";

  const strip = "shrink-0 flex items-center gap-1 px-2 py-1.5 bg-surface-850 border-t border-surface-700/20";

  const arrowHint = (axis: DragAxis) =>
    axis !== "vertical" ? (
      <span
        aria-hidden="true"
        className="absolute bottom-0.5 left-1/2 -translate-x-1/2 font-mono text-[9px] text-brand-400"
      >
        ←→
      </span>
    ) : null;

  return (
    <div
      className={strip}
      // Prevent toolbar taps from stealing focus away from the proxy input.
      // Without this, every button tap blurs the proxy and iOS closes the
      // soft keyboard. onClick handlers still fire normally.
      onMouseDown={(e) => e.preventDefault()}
    >
      <button type="button" aria-label="Arrow up" className={btnBase} {...upHandlers}>
        <span className="font-mono text-sm">{"\u2191"}</span>
        {arrowHint(upAxis)}
      </button>
      <button type="button" aria-label="Arrow down" className={btnBase} {...downHandlers}>
        <span className="font-mono text-sm">{"\u2193"}</span>
        {arrowHint(downAxis)}
      </button>
      <button type="button" aria-label="Tab" className={btnBase} onClick={() => send("\t")}>
        <span className="font-mono text-sm">Tab</span>
      </button>
      <button type="button" aria-label="Escape" className={btnBase} onClick={() => send("\x1b")}>
        <span className="font-mono text-sm">Esc</span>
      </button>
      <button
        type="button"
        aria-label="Ctrl"
        aria-pressed={ctrlActive}
        className={ctrlActive ? `${btnBase.replace("text-text-secondary", "text-brand-400")} bg-brand-600/20` : btnBase}
        onClick={() => {
          haptic();
          onCtrlToggle();
        }}
      >
        <span className="font-mono text-xs">Ctrl</span>
      </button>
      <button
        type="button"
        aria-label="Ctrl+C interrupt"
        className={btnBase}
        onClick={() => {
          send("\x03");
          if (ctrlActive) onCtrlToggle();
        }}
      >
        <span className="font-mono text-xs">^C</span>
      </button>
      <button
        type="button"
        aria-label="Paste from clipboard"
        className={btnBase}
        onClick={async () => {
          haptic();
          const t = toastBus.handler;
          if (!window.isSecureContext) {
            // No Clipboard API on a plain-HTTP origin. WebKit still honours
            // execCommand("paste") from a tap, behind its own Paste prompt,
            // when an editable is focused: the paste event lands on the
            // terminal's input, whose handler brackets it. Other engines
            // return false. Must run before any await to stay in the gesture.
            const active = document.activeElement;
            const editable = active instanceof HTMLTextAreaElement || active instanceof HTMLInputElement;
            if (keyboardOpen && editable && execCommandPaste()) return;
            t?.error("Paste needs HTTPS. Run `aoe serve --remote` for a Tailscale or Cloudflare HTTPS URL.");
            return;
          }
          const text = await readClipboardText();
          if (text) {
            sendData(bracketedPaste(text));
            return;
          }
          t?.error("Couldn't read clipboard. Try copying again, or open this dashboard in Safari.");
        }}
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <rect x="9" y="2" width="6" height="4" rx="1" />
          <path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2" />
        </svg>
      </button>
    </div>
  );
}
