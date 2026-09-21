// Persist the auth token in localStorage because iOS PWA launches drop `?token=` and may lose cookies. Accepted XSS trade-off for a small self-hosted app.

const STORAGE_KEY = "aoe_auth_token";

function captureFromUrl(): void {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  const token = url.searchParams.get("token");
  if (!token) return;

  try {
    // Token writes are load-bearing: on quota / SecurityError the auth
    // header survives via the cookie + URL fallback below, so raw setItem
    // stays here rather than routing through safeSetItem.
    // eslint-disable-next-line no-restricted-syntax
    window.localStorage.setItem(STORAGE_KEY, token);
  } catch {
    // Private mode: the token stays in the URL and cookie for this session.
    return;
  }

  url.searchParams.delete("token");
  const clean = url.pathname + (url.search ? url.search : "") + url.hash;
  window.history.replaceState(null, "", clean || "/");
}

captureFromUrl();

export function getToken(): string | null {
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

// Adopt a token the server rotated via X-Aoe-Token.
export function saveToken(token: string): void {
  const trimmed = token.trim();
  if (!trimmed) return;
  try {
    // Rotated tokens share the same fallback path as captureFromUrl;
    // raw setItem stays here for the documented rethrow contract.
    // eslint-disable-next-line no-restricted-syntax
    window.localStorage.setItem(STORAGE_KEY, trimmed);
  } catch {
    // The prompting request already succeeded on its cookie or header.
  }
}

export function clearToken(): void {
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    // nothing to do
  }
}
