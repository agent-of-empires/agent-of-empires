// Open an artifact through the token-injecting fetch, since a bare navigation lacks the Authorization header. The blob URL is left to the new tab.
export async function openArtifactInNewTab(url: string): Promise<void> {
  // Open synchronously while the click's activation is live, or the popup is blocked. `noopener` would make window.open return null.
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
      // Popup blocked anyway; last-resort direct open.
      window.open(objectUrl, "_blank", "noopener,noreferrer");
    }
  } catch {
    // A failed open is non-destructive.
    tab?.close();
  }
}
