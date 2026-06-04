import { isServerDown } from "./connectionState";
import { getOrCreateDeviceBindingSecret } from "./deviceBinding";
import { reportError } from "./toastBus";
import { clearToken, getToken, saveToken } from "./token";

/** Dispatched on `window` when the auth token is rejected or missing. App.tsx
 *  listens for this to show the token entry page instead of just a toast. */
export const TOKEN_EXPIRED_EVENT = "aoe:token-expired";

/** Dispatched on `window` when the token is valid but the passphrase login
 *  session is missing or expired. App.tsx listens to show the LoginPage
 *  instead of the TokenEntryPage, so a valid token isn't wrongly cleared. */
export const LOGIN_REQUIRED_EVENT = "aoe:login-required";

/** Dispatched on `window` when an authenticated request hits a sensitive
 *  route whose login session is not currently elevated (the server
 *  returns `403 elevation_required`). Structured view/terminal hooks listen for
 *  this to pop an inline passphrase prompt. See #1131. */
export const ELEVATION_REQUIRED_EVENT = "aoe:elevation-required";

/** Classify a 401 body as `login_required` or `unauthorized`. Clones the
 *  response so downstream readers (fetchJson, etc.) can still parse the
 *  body. Non-401 returns null. */
export async function classifyAuthError(
  res: Response,
): Promise<"login_required" | "unauthorized" | null> {
  if (res.status !== 401) return null;
  try {
    const data = (await res.clone().json()) as { error?: unknown };
    if (data && data.error === "login_required") return "login_required";
  } catch {
    // Body wasn't JSON or already consumed; fall through to unauthorized.
  }
  return "unauthorized";
}

/** Login attempt endpoints surface their own errors via LoginPage /
 *  ElevationPrompt local state. A 401 `unauthorized` from these paths
 *  means the passphrase the user just typed was wrong, not that the
 *  bearer token went stale. Firing the global `TOKEN_EXPIRED_EVENT`
 *  here would replace LoginPage with TokenEntryPage, leaving the user
 *  stuck on a token-entry screen in `--auth=passphrase` mode where no
 *  token URL exists. `login_required` from /api/login/elevate (session
 *  missing or expired during elevation) is *not* exempt: it still
 *  needs to surface as the global LoginPage swap. */
export function isLoginAttemptPath(path: string): boolean {
  return path === "/api/login" || path === "/api/login/elevate";
}

/**
 * Install a global fetch wrapper that:
 * 1. Injects `Authorization: Bearer <token>` when we have a stored token.
 *    The PWA needs this because iOS `start_url` strips the `?token=` query
 *    param on home-screen relaunch, and cookies can be lost across the
 *    Safari→standalone context switch.
 * 2. Reads `X-Aoe-Token` from same-origin responses and updates localStorage,
 *    so PWA clients stay in sync when the server rotates the token (the
 *    cookie flow gets this via `Set-Cookie`).
 * 3. Clears the stored token on 401 from `/api/*` so the PWA doesn't keep
 *    re-sending a dead token and wedging the user into a silent loop.
 * 4. Surfaces 5xx responses and network failures as user-visible toasts.
 *    4xx is intentionally silent because many endpoints treat client errors
 *    as part of normal validation (e.g. the wizard filesystem browser 400s
 *    on invalid paths while typing).
 *
 * Safe to call multiple times; only the first call installs the wrapper.
 */
export function installFetchErrorToasts(): void {
  if ((window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched) {
    return;
  }
  (window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched = true;

  const original = window.fetch.bind(window);

  window.fetch = async (input, init) => {
    const rawUrl =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.toString()
          : input.url;
    const path = toPath(rawUrl);
    const isApi = path.startsWith("/api/");
    const sameOrigin = isSameOrigin(rawUrl);

    // Generate a per-request id and inject as X-Request-Id so the backend
    // `http.request` middleware echoes the same id on its span. With it,
    // a network entry seen in devtools can be grep-correlated against the
    // backend log via `request_id=...`.
    let patchedInit = attachAuthHeader(sameOrigin, init);
    if (sameOrigin && isApi) {
      try {
        const requestId =
          typeof crypto !== "undefined" && "randomUUID" in crypto
            ? crypto.randomUUID()
            : Math.random().toString(36).slice(2);
        const h = new Headers(patchedInit?.headers ?? init?.headers);
        if (!h.has("X-Request-Id")) {
          h.set("X-Request-Id", requestId);
        }
        patchedInit = { ...(patchedInit ?? init ?? {}), headers: h };
      } catch {
        // Fall through without a request id; the middleware generates one.
      }
    }

    try {
      const res = await original(input, patchedInit);
      if (sameOrigin) {
        const rotated = res.headers.get("x-aoe-token");
        if (rotated) saveToken(rotated);
      }
      if (res.status === 401 && isApi) {
        const authError = await classifyAuthError(res);
        if (authError === "login_required") {
          // Session missing or expired (including mid-elevation):
          // always pop LoginPage. The token is still valid.
          handleLoginRequired();
        } else if (authError === "unauthorized" && !isLoginAttemptPath(path)) {
          // Generic 401 on a non-login path means the token is dead.
          // Login attempt paths surface their own wrong-passphrase error
          // through LoginPage / ElevationPrompt local state.
          handleTokenAuthFailure();
        }
      }
      if (res.status === 403 && isApi) {
        // `elevation_required` signals a sensitive route called without
        // a current 15-min passphrase confirmation. The structured view/terminal
        // surfaces listen for this and pop the inline prompt.
        try {
          const data = (await res.clone().json()) as { error?: unknown };
          if (data && data.error === "elevation_required") {
            window.dispatchEvent(new CustomEvent(ELEVATION_REQUIRED_EVENT));
          }
        } catch {
          // body not JSON; ignore.
        }
      }
      if (isApi && res.status >= 500 && !isServerDown()) {
        reportError(`Server error ${res.status} from ${path}`);
      }
      return res;
    } catch (err) {
      // Ignore aborts (triggered by deliberate cleanup).
      if (
        err instanceof DOMException &&
        (err.name === "AbortError" || err.name === "TimeoutError")
      ) {
        throw err;
      }
      // When the server is known to be down, suppress per-request toasts.
      // The DisconnectBanner handles the user-facing notification instead.
      if (isApi && !isServerDown()) {
        reportError(
          `Network error contacting ${path}. Check your connection.`,
        );
      }
      throw err;
    }
  };
}

// 401 with no `login_required` body: this is the true
// unauthenticated state. A bound device authenticates purely via
// its `aoe_session` cookie + device binding; the server does not
// consult the token on regular requests. This branch fires only
// when the session is gone (server restart, 30-day idle, explicit
// logout, or a fresh browser profile). Clear any cached token
// (the SPA may have a stale one in localStorage) and route the
// user back through the bootstrap flow. Dedupe so a burst of
// concurrent 401s produces one event. See #1167.
let tokenExpiredDispatched = false;
function handleTokenAuthFailure(): void {
  clearToken();
  if (tokenExpiredDispatched) return;
  tokenExpiredDispatched = true;
  window.dispatchEvent(new CustomEvent(TOKEN_EXPIRED_EVENT));
}

// On 401 `login_required` the token is fine; only the second factor is
// missing. Don't clear the token. Dedupe so a burst of concurrent 401s
// produces one event.
let loginRequiredDispatched = false;
function handleLoginRequired(): void {
  if (loginRequiredDispatched) return;
  loginRequiredDispatched = true;
  window.dispatchEvent(new CustomEvent(LOGIN_REQUIRED_EVENT));
}

/** Reset the dedup flags so a new 401 after re-authentication will be
 *  caught again. Called when the user submits a new token or completes
 *  the passphrase login. */
export function resetTokenExpired(): void {
  tokenExpiredDispatched = false;
  loginRequiredDispatched = false;
}

// Inject Authorization + device-binding headers without clobbering
// anything the caller set. Skips cross-origin URLs so we never leak
// either credential off-site.
function attachAuthHeader(
  sameOrigin: boolean,
  init: RequestInit | undefined,
): RequestInit | undefined {
  if (!sameOrigin) return init;
  const token = getToken();
  let bindingSecret: string | null = null;
  try {
    bindingSecret = getOrCreateDeviceBindingSecret();
  } catch {
    // Storage / crypto unavailable; login page will surface the error.
    // Leave the header off so the server's bad-request branch fires
    // instead of a silently broken auth state.
  }
  if (!token && !bindingSecret) return init;

  const headers = new Headers(init?.headers);
  if (token && !headers.has("Authorization")) {
    headers.set("Authorization", `Bearer ${token}`);
  }
  if (bindingSecret && !headers.has("X-Aoe-Device-Binding")) {
    headers.set("X-Aoe-Device-Binding", bindingSecret);
  }
  return { ...(init ?? {}), headers };
}

function isSameOrigin(url: string): boolean {
  if (url.startsWith("/")) return true;
  try {
    return new URL(url, window.location.origin).origin === window.location.origin;
  } catch {
    return false;
  }
}

/** Normalize any fetch input to a pathname so `/api/` checks work regardless
 *  of whether the caller passed a string, URL, or Request. */
function toPath(url: string): string {
  if (url.startsWith("/")) return url;
  try {
    return new URL(url, window.location.origin).pathname;
  } catch {
    return url;
  }
}
