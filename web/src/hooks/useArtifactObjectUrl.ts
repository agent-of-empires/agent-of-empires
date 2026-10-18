import { useEffect, useState } from "react";

/** Loads `url` through the auth-patched global fetch into a blob URL; a bare <img src> would carry no token. */
export function useArtifactObjectUrl(url: string): string | null {
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
