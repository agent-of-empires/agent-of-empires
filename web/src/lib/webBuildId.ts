// Dashboard build identity, used to detect a stale client after the
// `aoe` binary (which embeds the web bundle) updates underneath a
// long-lived page. Installed PWAs are the motivating case: iOS resumes
// the same page for weeks and offers no refresh affordance, so without
// an explicit prompt a phone keeps running old dashboard code (and old
// bugs) indefinitely. See DashboardUpdateBanner.

/** The content-hashed entry bundle name (`index-<hash>.js`) this page
 *  booted from, read off its own <script type="module"> tag. Returns
 *  null on the Vite dev server (entry is `/src/main.tsx`), which
 *  disables the staleness check there. */
export function currentWebBuildId(doc: Document = document): string | null {
  for (const script of Array.from(doc.querySelectorAll<HTMLScriptElement>("script[type=module][src]"))) {
    const m = script.src.match(/\/assets\/(index-[A-Za-z0-9_-]+\.js)(?:\?|$)/);
    if (m) return m[1]!;
  }
  return null;
}

/** True when the server reports a different embedded bundle than the
 *  one this page booted from. Either side missing disables the check
 *  (dev server, or an older `aoe` that doesn't report web_build_id). */
export function isWebUpdateAvailable(current: string | null, server: string | null | undefined): boolean {
  return !!current && !!server && current !== server;
}
