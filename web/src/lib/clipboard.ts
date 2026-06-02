/** Write `text` to the clipboard, returning whether it succeeded.
 *
 *  Prefers the async Clipboard API, but that is only defined in secure
 *  contexts (HTTPS or `localhost`). `aoe serve` is frequently reached
 *  over plain HTTP on a LAN or Tailscale IP, where `navigator.clipboard`
 *  is `undefined`, so fall back to a hidden-textarea `execCommand("copy")`
 *  (same approach the mobile terminal toolbar uses for its paste path). */
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
