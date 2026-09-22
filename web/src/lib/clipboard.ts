/** Bracketed paste so newlines stay one paste. A pane without DECSET 2004 shows the markers as text. */
export function bracketedPaste(text: string): string {
  return `\x1b[200~${text}\x1b[201~`;
}

const CLIPBOARD_TEXT_TYPES = ["text/plain", "text/uri-list", "text/html"] as const;

// Copy-link UIs often write only text/uri-list.
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

/** "" when refused or empty; the async Clipboard API needs a secure context. */
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

/** Falls back to `execCommand("copy")` over plain HTTP, where `navigator.clipboard` is undefined. */
export async function writeClipboard(text: string): Promise<boolean> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Permission denied or no focus: try execCommand.
    }
  }
  return legacyCopy(text);
}

export interface ArmedClipboardWrite {
  resolve: (text: string) => boolean;
  cancel: () => void;
}

/** Arm a write during a user gesture and resolve it when OSC 52 arrives; a promise-valued ClipboardItem keeps the gesture's authorization. */
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
      // An unconsumed timed-out promise must not become an unhandled rejection.
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
    // Promise-valued ClipboardItem unsupported.
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
  // Off-screen and read-only so selecting it neither scrolls nor visibly steals focus.
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
