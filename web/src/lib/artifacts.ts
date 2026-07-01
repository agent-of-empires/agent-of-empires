// Open a session artifact (served by the authenticated artifact route) in a
// new tab. The dashboard's global `fetch` is patched to inject the auth token
// (see fetchInterceptor.ts), so we fetch the bytes and open the resulting blob;
// a bare new-tab navigation would miss the Authorization header in token-auth
// mode. See #2587.
//
// ponytail: the object URL is not revoked; the new tab owns its lifetime and
// leaking one blob URL per user click is not worth tracking cross-tab.
export async function openArtifactInNewTab(url: string): Promise<void> {
  // Open the tab synchronously, while the click's user activation is still
  // live, then point it at the blob once the bytes arrive. Opening after the
  // await would count as a programmatic popup and get blocked (Safari is
  // strictest). No `noopener` here: it makes window.open return null, losing
  // the reference we need to set location, and the artifact is same-origin
  // content we fetched ourselves, not a cross-origin tabnabbing vector.
  const tab = window.open("about:blank", "_blank");
  try {
    const r = await fetch(url);
    if (!r.ok) {
      tab?.close();
      return;
    }
    const blob = await r.blob();
    const objectUrl = URL.createObjectURL(blob);
    if (tab) {
      tab.location.href = objectUrl;
    } else {
      // Popup blocked despite the sync open; last-resort direct open.
      window.open(objectUrl, "_blank", "noopener,noreferrer");
    }
  } catch {
    // Swallow: a failed artifact open is non-destructive; nothing to recover.
    tab?.close();
  }
}
