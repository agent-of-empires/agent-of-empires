// The same owner name on another forge is a different account, so only GitHub remotes get an avatar.
const GITHUB_HOSTS = new Set(["github.com", "ssh.github.com"]);

/** The GitHub avatar for an `owner@host` remote owner key, or null when the host is not GitHub. */
export function githubAvatar(key: string | null, size: number): { url: string; owner: string } | null {
  if (!key) return null;
  // Owner names cannot contain `@`, so the last one splits owner from host.
  const at = key.lastIndexOf("@");
  if (at <= 0) return null;
  const host = key
    .slice(at + 1)
    .toLowerCase()
    .replace(/:\d+$/, "");
  if (!GITHUB_HOSTS.has(host)) return null;
  const owner = key.slice(0, at);
  return { url: `https://github.com/${encodeURIComponent(owner)}.png?size=${size * 2}`, owner };
}
