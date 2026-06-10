import { useCallback, useState } from "react";
import type { RefObject } from "react";
import { useLongPressDrag, type DragAxis } from "../hooks/useLongPressDrag";
import { toastBus } from "../lib/toastBus";

const CLIPBOARD_TEXT_TYPES = ["text/plain", "text/uri-list", "text/html"] as const;

// Normalize clipboard payloads to plain text. Necessary because GitHub's
// "Copy link" buttons (and many Mac copy-link UIs) write text/uri-list
// only, no text/plain, so the browser's default paste handler ends up
// with an empty payload.
function normalizeClipboardData(type: string, raw: string): string {
  if (type === "text/uri-list") {
    // text/uri-list permits multiple URLs separated by CRLF, with
    // comments starting with #. Drop comments, join with newlines.
    return raw
      .split(/\r?\n/)
      .filter((l) => l && !l.startsWith("#"))
      .join("\n");
  }
  if (type === "text/html") {
    const doc = new DOMParser().parseFromString(raw, "text/html");
    const anchor = doc.querySelector("a[href]");
    const href = anchor?.getAttribute("href");
    if (href) return href;
    return doc.body?.textContent?.trim() ?? "";
  }
  return raw;
}

function extractClipboardText(cd: DataTransfer | null): string {
  if (!cd) return "";
  for (const ty of CLIPBOARD_TEXT_TYPES) {
    const raw = cd.getData(ty);
    if (raw) {
      const normalized = normalizeClipboardData(ty, raw);
      if (normalized) return normalized;
    }
  }
  return "";
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

          // Path A: Clipboard API. Doesn't require focus, so it doesn't
          // pop the soft keyboard. Tries every MIME type the source app
          // wrote (e.g. GitHub's Copy Link writes text/uri-list only,
          // not text/plain, which is why the old execCommand path read
          // empty). HTTPS-only on iOS.
          if (window.isSecureContext) {
            try {
              if (navigator.clipboard?.read) {
                const items = await navigator.clipboard.read();
                for (const item of items) {
                  for (const ty of CLIPBOARD_TEXT_TYPES) {
                    if (!item.types.includes(ty)) continue;
                    const blob = await item.getType(ty);
                    const raw = await blob.text();
                    const text = normalizeClipboardData(ty, raw);
                    if (text) {
                      sendData(text);
                      return;
                    }
                  }
                }
              } else if (navigator.clipboard?.readText) {
                const text = await navigator.clipboard.readText();
                if (text) {
                  sendData(text);
                  return;
                }
              }
            } catch {
              // Permission denied, no focus, etc. Fall through to path B.
            }
          }

          // Path B: execCommand-based fallback for insecure contexts.
          //
          // Keyboard-open branch: reuse whatever editable element is
          // already focused as the paste target. We never call focus(),
          // so iOS sees no focus transition and the soft keyboard stays
          // up. execCommand("paste") fires the paste event on the active
          // element, our listener reads clipboardData directly.
          //
          // Keyboard-closed branch: there's no editable focused, so we
          // have to focus the terminal textarea ourselves. Flip it to
          // readonly first so iOS doesn't pop the keyboard, then blur
          // afterward so the next FAB tap is a real focus transition.
          const activeEl = document.activeElement;
          const activeIsEditable = activeEl instanceof HTMLTextAreaElement || activeEl instanceof HTMLInputElement;

          if (keyboardOpen && activeIsEditable) {
            let recovered = "";
            const onPaste: EventListener = (e: Event) => {
              recovered = extractClipboardText((e as ClipboardEvent).clipboardData);
            };
            activeEl.addEventListener("paste", onPaste, { once: true });
            try {
              document.execCommand("paste");
            } catch {
              // continue to error toast
            }
            activeEl.removeEventListener("paste", onPaste);
            if (recovered) {
              sendData(recovered);
              return;
            }
          } else {
            const ta = inputElRef.current;
            if (ta) {
              let recovered = "";
              const onPaste = (e: ClipboardEvent) => {
                recovered = extractClipboardText(e.clipboardData);
              };
              ta.addEventListener("paste", onPaste, { once: true });
              // Attribute (not property) toggles keep the react-hooks
              // immutability lint happy about ref-derived elements.
              const prevReadOnly = ta.hasAttribute("readonly");
              ta.setAttribute("readonly", "");
              try {
                ta.focus({ preventScroll: true });
                document.execCommand("paste");
              } catch {
                // continue to error toast
              }
              if (!prevReadOnly) ta.removeAttribute("readonly");
              ta.blur();
              ta.removeEventListener("paste", onPaste);
              if (recovered) {
                sendData(recovered);
                return;
              }
            }
          }

          // All paths failed. Tell the user what to try next.
          if (!window.isSecureContext) {
            t?.error("Paste needs HTTPS. Run `aoe serve --remote` for a Tailscale or Cloudflare HTTPS URL.");
          } else {
            t?.error("Couldn't read clipboard. Try copying again, or open this dashboard in Safari.");
          }
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
