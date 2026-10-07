/** `status` is the HTTP status of a refused fetch; it is absent for a network error. */
export type OpenResult = { ok: true } | { ok: false; status?: number };

// Firefox can cancel a download whose blob URL is revoked in the same task as the click.
const REVOKE_DOWNLOAD_URL_MS = 40_000;

// Open a same-origin file through the token-injecting fetch, since a bare navigation lacks the Authorization header. The blob URL is left to the new tab.
export async function openInNewTab(url: string, downloadName?: string): Promise<OpenResult> {
  // Open synchronously while the click's activation is live, or the popup is blocked. `noopener` would make window.open return null.
  const tab = window.open("about:blank", "_blank");
  try {
    const r = await fetch(url);
    if (!r.ok) {
      tab?.close();
      return { ok: false, status: r.status };
    }
    const blob = await r.blob();
    const objectUrl = URL.createObjectURL(blob);
    // A blob URL drops Content-Disposition, so save an attachment here under its own name instead of an anonymous blob.
    if (r.headers.get("Content-Disposition")?.startsWith("attachment")) {
      tab?.close();
      const link = document.createElement("a");
      link.href = objectUrl;
      link.download = downloadName ?? lastPathSegment(url);
      link.click();
      setTimeout(() => URL.revokeObjectURL(objectUrl), REVOKE_DOWNLOAD_URL_MS);
      return { ok: true };
    }
    if (tab) {
      tab.location.href = objectUrl;
    } else {
      // Popup blocked anyway; last-resort direct open.
      window.open(objectUrl, "_blank", "noopener,noreferrer");
    }
    return { ok: true };
  } catch {
    // A failed open is non-destructive.
    tab?.close();
    return { ok: false };
  }
}

function lastPathSegment(url: string): string {
  const path = url.split(/[?#]/, 1)[0] ?? "";
  return decodeURIComponent(path.slice(path.lastIndexOf("/") + 1));
}
