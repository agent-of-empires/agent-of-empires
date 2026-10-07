import { isServerDown } from "./connectionState";
import { getOrCreateDeviceBindingSecret } from "./deviceBinding";
import { reportError } from "./toastBus";
import { clearToken, getToken, saveToken } from "./token";

const LIFECYCLE_NETWORK_TOAST_SUPPRESS_MS = 2_000;

/** The token was rejected or is missing; App shows the token entry page. */
export const TOKEN_EXPIRED_EVENT = "aoe:token-expired";

/** The token is valid but the login session is missing; App shows LoginPage without clearing the token. */
export const LOGIN_REQUIRED_EVENT = "aoe:login-required";

/** A sensitive route needs a fresh passphrase (`403 elevation_required`). */
export const ELEVATION_REQUIRED_EVENT = "aoe:elevation-required";

/** Classify a 401 body without consuming it. Non-401 returns null. */
export async function classifyAuthError(res: Response): Promise<"login_required" | "unauthorized" | null> {
  if (res.status !== 401) return null;
  try {
    const data = (await res.clone().json()) as { error?: unknown };
    if (data && data.error === "login_required") return "login_required";
  } catch {
    // Not JSON or already consumed: unauthorized.
  }
  return "unauthorized";
}

/** A 401 from a login attempt means a wrong passphrase, not a stale token, so it must not fire `TOKEN_EXPIRED_EVENT`. */
export function isLoginAttemptPath(path: string): boolean {
  return path === "/api/login" || path === "/api/login/elevate";
}

const ACP_PROMPT_PATH = /^\/api\/sessions\/[^/]+\/acp\/prompt$/;

/** The retryable `worker_not_ready` 503 of a prompt POST to a resuming worker. Reads a clone. */
async function isTransientWorkerNotReady(res: Response, path: string): Promise<boolean> {
  if (res.status !== 503 || !ACP_PROMPT_PATH.test(path)) return false;
  try {
    const body = await res.clone().text();
    return body.startsWith("worker_not_ready");
  } catch {
    return false;
  }
}

/** Install a global fetch wrapper (idempotent) that attaches same-origin auth headers (iOS PWA relaunch drops `?token=`), adopts rotated `X-Aoe-Token`, routes 401/403 auth failures, and toasts 5xx and network failures. 4xx stays silent since many endpoints use it for validation. */
export function installFetchErrorToasts(): void {
  if ((window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched) {
    return;
  }
  (window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched = true;
  installPageLifecycleTracking();

  const original = window.fetch.bind(window);

  window.fetch = async (input, init) => {
    const rawUrl = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const path = toPath(rawUrl);
    const isApi = path.startsWith("/api/");
    const sameOrigin = isSameOrigin(rawUrl);

    // X-Request-Id correlates a devtools entry with the backend `http.request` span.
    let patchedInit = attachAuthHeader(sameOrigin, init);
    if (sameOrigin && isApi) {
      try {
        const requestId = crypto.randomUUID();
        const h = new Headers(patchedInit?.headers ?? init?.headers);
        if (!h.has("X-Request-Id")) {
          h.set("X-Request-Id", requestId);
        }
        patchedInit = { ...(patchedInit ?? init ?? {}), headers: h };
      } catch {
        // Without a request id the middleware generates one.
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
          handleLoginRequired();
        } else if (authError === "unauthorized" && !isLoginAttemptPath(path)) {
          // A generic 401 outside login attempts means the token is dead.
          handleTokenAuthFailure();
        }
      }
      if (res.status === 403 && isApi) {
        try {
          const data = (await res.clone().json()) as { error?: unknown };
          if (data && data.error === "elevation_required") {
            window.dispatchEvent(new CustomEvent(ELEVATION_REQUIRED_EVENT));
          }
        } catch {
          // Body not JSON; ignore.
        }
      }
      if (isApi && res.status >= 500 && !isServerDown()) {
        // The structured view treats this 503 as "queued, will retry", so don't toast it.
        if (await isTransientWorkerNotReady(res, path)) {
          return res;
        }
        reportError(`Server error ${res.status} from ${path}`);
      }
      return res;
    } catch (err) {
      if (err instanceof DOMException && (err.name === "AbortError" || err.name === "TimeoutError")) {
        throw err;
      }
      // The DisconnectBanner covers a known-down server.
      if (isApi && !isServerDown() && !isPageLifecycleNetworkGlitch()) {
        reportError(`Network error contacting ${path}. Check your connection.`);
      }
      throw err;
    }
  };
}

// A plain 401 means the session is gone (bound devices auth by cookie), so clear any stale token. Deduped.
let tokenExpiredDispatched = false;
function handleTokenAuthFailure(): void {
  clearToken();
  if (tokenExpiredDispatched) return;
  tokenExpiredDispatched = true;
  window.dispatchEvent(new CustomEvent(TOKEN_EXPIRED_EVENT));
}

// The token is fine; only the second factor is missing. Deduped.
let loginRequiredDispatched = false;
function handleLoginRequired(): void {
  if (loginRequiredDispatched) return;
  loginRequiredDispatched = true;
  window.dispatchEvent(new CustomEvent(LOGIN_REQUIRED_EVENT));
}

let lastPageLifecycleChangeAt = 0;
let lifecycleTrackingInstalled = false;

function installPageLifecycleTracking(): void {
  if (lifecycleTrackingInstalled) return;
  lifecycleTrackingInstalled = true;
  const mark = () => {
    lastPageLifecycleChangeAt = Date.now();
  };
  document.addEventListener("visibilitychange", mark);
  window.addEventListener("pagehide", mark);
  window.addEventListener("pageshow", mark);
  window.addEventListener("blur", mark);
  window.addEventListener("focus", mark);
}

function isPageLifecycleNetworkGlitch(): boolean {
  if (document.visibilityState === "hidden") return true;
  return lastPageLifecycleChangeAt > 0 && Date.now() - lastPageLifecycleChangeAt < LIFECYCLE_NETWORK_TOAST_SUPPRESS_MS;
}

/** Re-arm the dedupe after re-authentication. */
export function resetTokenExpired(): void {
  tokenExpiredDispatched = false;
  loginRequiredDispatched = false;
}

// Skips cross-origin URLs so credentials never leak off-site.
function attachAuthHeader(sameOrigin: boolean, init: RequestInit | undefined): RequestInit | undefined {
  if (!sameOrigin) return init;
  const token = getToken();
  let bindingSecret: string | null = null;
  try {
    bindingSecret = getOrCreateDeviceBindingSecret();
  } catch {
    // Storage or crypto unavailable; leave the header off so the server reports it.
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

function toPath(url: string): string {
  if (url.startsWith("/")) return url;
  try {
    return new URL(url, window.location.origin).pathname;
  } catch {
    return url;
  }
}
