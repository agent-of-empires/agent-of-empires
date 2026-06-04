import { useState } from "react";

/** Renders the GitHub avatar for a repo owner. Hides itself on load error. */
export function OwnerAvatar({
  owner,
  size = 16,
}: {
  owner: string | null;
  size?: number;
}) {
  const [erroredOwner, setErroredOwner] = useState<string | null>(null);
  const hidden = owner === erroredOwner;

  if (!owner || hidden) return null;

  return (
    <img
      src={`https://github.com/${owner}.png?size=${size * 2}`}
      alt={owner}
      width={size}
      height={size}
      className="rounded-sm shrink-0"
      onError={() => setErroredOwner(owner)}
    />
  );
}
