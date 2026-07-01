// Inline display of session artifacts served by the authenticated artifact
// route. The dashboard's global `fetch` is patched to inject the auth token
// (see fetchInterceptor.ts), so a plain `fetch(url)` carries credentials; a
// bare <img src> would not. We therefore fetch the bytes into a blob object
// URL and render that. Opening artifacts in a new tab lives in
// lib/artifacts.ts. See #2587.

import { useEffect, useState } from "react";

/** Fetch `url` (through the authed global fetch) into a blob object URL.
 *  Returns the object URL once loaded, or `null` while loading or on error.
 *  Revokes the URL on unmount / url change. */
function useArtifactObjectUrl(url: string): string | null {
  const [objectUrl, setObjectUrl] = useState<string | null>(null);

  useEffect(() => {
    let revoked = false;
    let created: string | null = null;
    fetch(url)
      .then((r) => (r.ok ? r.blob() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((blob) => {
        if (revoked) return;
        created = URL.createObjectURL(blob);
        setObjectUrl(created);
      })
      .catch(() => {
        if (!revoked) setObjectUrl(null);
      });
    return () => {
      revoked = true;
      if (created) URL.revokeObjectURL(created);
      setObjectUrl(null);
    };
  }, [url]);

  return objectUrl;
}

/** Inline image for an artifact route URL. Shows the alt text as a muted
 *  placeholder until the bytes load, and keeps it if the fetch fails, so a
 *  failed artifact never renders as a broken image icon. */
export function ArtifactImage({ url, alt }: { url: string; alt?: string }) {
  const objectUrl = useArtifactObjectUrl(url);
  if (!objectUrl) {
    return <span className="acp-inert-path">{alt || "artifact"}</span>;
  }
  return <img className="acp-artifact-image" src={objectUrl} alt={alt ?? ""} />;
}
