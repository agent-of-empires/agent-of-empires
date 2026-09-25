// Classification and dispatch for plugin-supplied hrefs. Plugin strings are
// untrusted, so only http(s) URLs and same-origin relative paths ever become
// links. Mirrors the backend `is_allowed_href` in src/util.rs.

import { requestNavigate, requestOpenSession } from "./sessionRoute";

const HTTP_RE = /^https?:\/\//i;

function isRelativePath(href: string): boolean {
  // A leading `//` is scheme-relative and resolves to a different host, not a path. A
  // backslash or embedded tab/CR/LF must also be rejected: browsers normalize `\` to `/`
  // for special schemes and strip tab/CR/LF anywhere in the string, so e.g. "/\evil.com"
  // or "/\n/evil.com" would otherwise pass this check but resolve to a different origin.
  if (/[\\\t\r\n]/.test(href)) return false;
  return href.startsWith("/") && !href.startsWith("//");
}

export function isAllowedHref(href: unknown): href is string {
  return typeof href === "string" && (HTTP_RE.test(href) || isRelativePath(href));
}

/** Whether `href` resolves to aoe's own origin, so it should be a client-side
 *  navigation instead of a new browser tab. Assumes `isAllowedHref(href)`. */
export function isInternalHref(href: string): boolean {
  if (isRelativePath(href)) return true;
  try {
    return new URL(href, window.location.origin).origin === window.location.origin;
  } catch {
    return false;
  }
}

/** `href`'s path, query and hash, for client-side navigation. Always resolved
 *  through `URL` (rather than returning a relative `href` unchanged) so dot-segments
 *  and encoding normalize exactly like a real browser navigation would, which a
 *  modified click on the same anchor falls through to. Assumes `isInternalHref(href)`. */
export function toInternalPath(href: string): string {
  const url = new URL(href, window.location.origin);
  return `${url.pathname}${url.search}${url.hash}`;
}

// Case-insensitive to match react-router's own `useMatch("/session/:sessionId")`
// default (so a differently-capitalized plugin path still gets the full
// session-selection flow instead of silently falling through to a bare
// `requestNavigate`). Anchored to the end (an optional trailing slash, then
// query/hash/end-of-string) so an extra path segment like `/session/xyz/extra`,
// which that same route would NOT match, isn't misread as a session link either.
const SESSION_PATH_RE = /^\/session\/([^/?#]+)\/?(?:[?#]|$)/i;

/** Navigates to an internal href via the same request the app already uses for
 *  that route: a session route reuses `requestOpenSession` (which also drives
 *  mobile keyboard-proxy transitions, input focus and closing the sidebar),
 *  anything else falls back to a plain router `requestNavigate`.
 *  Assumes `isInternalHref(href)`. */
export function navigateInternalHref(href: string): void {
  const path = toInternalPath(href);
  const rawSessionId = SESSION_PATH_RE.exec(path)?.[1];
  const sessionId = rawSessionId && decodeSessionId(rawSessionId);
  if (sessionId) {
    requestOpenSession(sessionId, path);
  } else {
    requestNavigate(path);
  }
}

/** `decodeURIComponent` throws on malformed percent-encoding (e.g. `%ZZ`); a
 *  plugin-supplied path can't be trusted not to send that. */
function decodeSessionId(raw: string): string | null {
  try {
    return decodeURIComponent(raw);
  } catch {
    return null;
  }
}
