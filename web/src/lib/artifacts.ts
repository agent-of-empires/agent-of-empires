// Open a session artifact (served by the authenticated artifact route) in a
// new tab. The dashboard's global `fetch` is patched to inject the auth token
// (see fetchInterceptor.ts), so we fetch the bytes and open the resulting blob;
// a bare new-tab navigation would miss the Authorization header in token-auth
// mode. See #2587.
//
// ponytail: the object URL is not revoked; the new tab owns its lifetime and
// leaking one blob URL per user click is not worth tracking cross-tab.
export async function openArtifactInNewTab(url: string): Promise<void> {
  try {
    const r = await fetch(url);
    if (!r.ok) return;
    const blob = await r.blob();
    window.open(URL.createObjectURL(blob), "_blank", "noopener,noreferrer");
  } catch {
    // Swallow: a failed artifact open is non-destructive; nothing to recover.
  }
}
