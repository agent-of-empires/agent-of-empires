// Detect a stale dashboard after the binary updates, mainly for installed PWAs that never refresh.

/** Read off this page's module script; null on the Vite dev server. */
export function currentWebBuildId(doc: Document = document): string | null {
  for (const script of Array.from(doc.querySelectorAll<HTMLScriptElement>("script[type=module][src]"))) {
    const m = script.src.match(/\/assets\/(index-[A-Za-z0-9_-]+\.js)(?:\?|$)/);
    if (m) return m[1]!;
  }
  return null;
}

/** Missing on either side disables the check. */
export function isWebUpdateAvailable(current: string | null, server: string | null | undefined): boolean {
  return !!current && !!server && current !== server;
}
