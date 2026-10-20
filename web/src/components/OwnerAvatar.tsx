import { useState, type ReactNode } from "react";
import { githubAvatar } from "../lib/githubAvatar";

/** A GitHub remote owner's avatar; `fallback` takes its place off GitHub or when the image fails to load. */
export function OwnerAvatar({
  ownerKey,
  fallback,
  size = 16,
}: {
  ownerKey: string | null;
  fallback: ReactNode;
  size?: number;
}) {
  const avatar = githubAvatar(ownerKey, size);
  const [failedUrl, setFailedUrl] = useState<string | null>(null);

  if (!avatar || avatar.url === failedUrl) return <>{fallback}</>;
  const { url, owner } = avatar;

  return (
    <img
      src={url}
      alt={owner}
      width={size}
      height={size}
      className="rounded-sm shrink-0"
      onError={() => setFailedUrl(url)}
    />
  );
}
