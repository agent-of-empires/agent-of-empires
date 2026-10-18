// Artifact images load through the auth-patched global fetch into a blob URL;
// a bare <img src> would carry no token.

import { useArtifactObjectUrl } from "../../hooks/useArtifactObjectUrl";

/** Alt text stands in while loading and on failure, never a broken image. */
export function ArtifactImage({ url, alt }: { url: string; alt?: string }) {
  const objectUrl = useArtifactObjectUrl(url);
  if (!objectUrl) {
    return <span className="acp-inert-path">{alt || "artifact"}</span>;
  }
  return <img className="acp-artifact-image" src={objectUrl} alt={alt ?? ""} />;
}
