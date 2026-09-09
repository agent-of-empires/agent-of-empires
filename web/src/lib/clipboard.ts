/** Wrap pasted text in bracketed-paste markers so an agent treats embedded
 *  newlines as part of one paste rather than per-line submits.
 *
 *  The web input path cannot see whether the pane enabled DECSET 2004, so a
 *  raw shell or simple REPL receives the markers as text and shows `00~` /
 *  `01~` around the paste. The TUI avoids this by pasting through tmux
 *  (`Session::paste_text`); routing web pastes the same way is the fix. */
export function bracketedPaste(text: string): string {
  return `\x1b[200~${text}\x1b[201~`;
}

const CLIPBOARD_TEXT_TYPES = ["text/plain", "text/uri-list", "text/html"] as const;

// GitHub's "Copy link" buttons (and many Mac copy-link UIs) write
// text/uri-list only, no text/plain, so a text/plain-only reader sees an
// empty clipboard.
function normalizeClipboardData(type: string, raw: string): string {
  if (type === "text/uri-list") {
    // CRLF-separated URLs with `#` comment lines.
    return raw
      .split(/\r?\n/)
      .filter((l) => l && !l.startsWith("#"))
      .join("\n");
  }
  if (type === "text/html") {
    const doc = new DOMParser().parseFromString(raw, "text/html");
    const href = doc.querySelector("a[href]")?.getAttribute("href");
    return href || (doc.body?.textContent?.trim() ?? "");
  }
  return raw;
}

/** Read the clipboard as plain text from a user gesture, or "" when the
 *  browser refuses or it is empty. The async Clipboard API exists only in
 *  secure contexts; iOS shows its own Paste confirmation and rejects if the
 *  user dismisses it. */
export async function readClipboardText(): Promise<string> {
  if (!window.isSecureContext) return "";
  try {
    if (navigator.clipboard?.read) {
      for (const item of await navigator.clipboard.read()) {
        for (const type of CLIPBOARD_TEXT_TYPES) {
          if (!item.types.includes(type)) continue;
          const text = normalizeClipboardData(type, await (await item.getType(type)).text());
          if (text) return text;
        }
      }
      return "";
    }
    return (await navigator.clipboard?.readText?.()) ?? "";
  } catch {
    return "";
  }
}

/** Write `text` to the clipboard, returning whether it succeeded.
 *
 *  Prefers the async Clipboard API, but that is only defined in secure
 *  contexts (HTTPS or `localhost`). `aoe serve` is frequently reached
 *  over plain HTTP on a LAN or Tailscale IP, where `navigator.clipboard`
 *  is `undefined`, so fall back to a hidden-textarea `execCommand("copy")`. */
export async function writeClipboard(text: string): Promise<boolean> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Permission denied, no document focus, etc. Fall through to the
      // execCommand path rather than failing outright.
    }
  }
  return legacyCopy(text);
}

export interface ArmedClipboardWrite {
  /** Resolve the gesture-bound write. Returns false after cancellation or timeout. */
  resolve: (text: string) => boolean;
  cancel: () => void;
}

/** Arm a clipboard write during a browser user gesture and resolve it later.
 *
 *  OSC 52 reaches the dashboard asynchronously after a mouse release has
 *  crossed the WebSocket and pane. Chromium and Safari preserve the release's
 *  clipboard authorization when `clipboard.write()` receives a
 *  promise-valued ClipboardItem synchronously; the promise is resolved when
 *  the OSC 52 payload arrives. Engines without that path fall back to the
 *  best-effort writer above. */
export function armClipboardWrite(timeoutMs = 1000): ArmedClipboardWrite {
  let settled = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let resolveText: ((text: string) => void) | null = null;
  let rejectBlob: ((reason?: unknown) => void) | null = null;

  const finish = () => {
    settled = true;
    if (timer) clearTimeout(timer);
    timer = null;
    resolveText = null;
    rejectBlob = null;
  };

  try {
    if (window.isSecureContext && typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
      let resolveBlob: ((blob: Blob) => void) | null = null;
      const pending = new Promise<Blob>((resolve, reject) => {
        resolveBlob = resolve;
        rejectBlob = reject;
      });
      // A timed-out selection must not become an unhandled rejection if an
      // engine drops the ClipboardItem's promise without consuming it.
      pending.catch(() => {});
      resolveText = (text) => resolveBlob?.(new Blob([text], { type: "text/plain" }));
      timer = setTimeout(() => {
        if (settled) return;
        const reject = rejectBlob;
        finish();
        reject?.(new Error("clipboard event timeout"));
      }, timeoutMs);
      void navigator.clipboard.write([new ClipboardItem({ "text/plain": pending })]).catch(() => {});
    }
  } catch {
    // Promise-valued ClipboardItem is not supported. Use the fallback below.
    if (timer) clearTimeout(timer);
    timer = null;
    resolveText = null;
  }

  if (!resolveText) {
    resolveText = (text) => {
      void writeClipboard(text);
    };
    timer = setTimeout(finish, timeoutMs);
  }

  return {
    resolve(text) {
      if (settled || !resolveText) return false;
      const resolve = resolveText;
      finish();
      resolve(text);
      return true;
    },
    cancel() {
      if (settled) return;
      const reject = rejectBlob;
      finish();
      reject?.(new Error("clipboard write cancelled"));
    },
  };
}

function legacyCopy(text: string): boolean {
  const ta = document.createElement("textarea");
  ta.value = text;
  // Keep it off-screen and non-interactive so selecting it neither scrolls
  // the page nor steals focus visibly.
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.top = "-9999px";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  try {
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    document.body.removeChild(ta);
  }
}
