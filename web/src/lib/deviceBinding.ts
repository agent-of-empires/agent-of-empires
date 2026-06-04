// Per-browser device-binding secret used as the second factor on top of
// the passphrase login cookie. Generated once on first load via
// `crypto.getRandomValues`, persisted in localStorage, and presented on
// every authenticated REST request (via `X-Aoe-Device-Binding`) and
// every WebSocket upgrade (via the `aoe-device.<secret>` subprotocol).
//
// A stolen `aoe_session` cookie alone is therefore not enough to
// authenticate as the user: the attacker also needs the binding
// secret. Mobile IP rotation no longer logs the user out because IP
// is no longer part of the session identity on the server side. See
// #1131.
//
// Generation timing: the secret is created on the FIRST authenticated
// fetch, not at login. `fetchInterceptor.attachAuthHeader` calls
// `getOrCreateDeviceBindingSecret()` for every same-origin request,
// and `loginStatus()` runs before the login page renders. So by the
// time the user submits the passphrase, the secret already exists in
// localStorage and the `device_binding_secret` field on the POST body
// is just re-reading it. Intentional: the binding is per-browser, not
// per-session, and rotating it with each login would force every PWA
// tab open on this browser to re-authenticate at the same moment.
//
// This module deliberately stays small so that a future hardening
// pass (WebCrypto non-extractable keys + per-request signatures) can
// replace the storage and accessor without touching the call sites.

const STORAGE_KEY = "aoe_device_binding_secret_v1";

/** Bytes of entropy in the secret. Matches the server-side constant in
 *  `src/server/login.rs::BINDING_SECRET_BYTES`. */
const BINDING_SECRET_BYTES = 32;

let cached: string | null = null;

/**
 * Return the persisted device-binding secret, generating and storing
 * one on first call. The returned value is the base64url-encoded
 * 32-byte secret ready to ship over the wire.
 *
 * Throws if `crypto.getRandomValues` or `localStorage` is unavailable;
 * the login page surfaces a typed error in that case rather than
 * silently falling back to a guessable identifier.
 */
export function getOrCreateDeviceBindingSecret(): string {
  if (cached !== null) return cached;
  try {
    const existing = window.localStorage.getItem(STORAGE_KEY);
    if (existing && isValidEncoded(existing)) {
      cached = existing;
      return existing;
    }
  } catch {
    // localStorage threw (Safari private mode, sandboxed iframe).
    // Fall through to generation; the write below will throw again
    // and the caller will surface the failure to the user.
  }
  const bytes = new Uint8Array(BINDING_SECRET_BYTES);
  if (typeof crypto === "undefined" || !crypto.getRandomValues) {
    throw new Error(
      "Browser does not expose crypto.getRandomValues; cannot create device binding",
    );
  }
  crypto.getRandomValues(bytes);
  const secret = base64UrlEncode(bytes);
  try {
    // Device binding must hard-fail on quota; callers surface the error
    // to the user rather than silently degrading. Stays on raw setItem.
    // eslint-disable-next-line no-restricted-syntax
    window.localStorage.setItem(STORAGE_KEY, secret);
  } catch (err) {
    throw new Error(
      `Could not persist device binding secret: ${describeError(err)}`,
      { cause: err },
    );
  }
  cached = secret;
  return secret;
}

/** Drop the cached secret. Called by the logout flow so a future
 *  login creates a fresh binding (the cookie is invalidated server
 *  side; the secret should rotate with it). */
export function clearDeviceBindingSecret(): void {
  cached = null;
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    // ignore
  }
}

/** Test seam: returns the in-memory cached secret without going
 *  through localStorage. Used by unit tests to assert that the
 *  module memoises after the first call. */
export function __getCachedDeviceBindingSecretForTests(): string | null {
  return cached;
}

/** Test seam: reset the memoised secret so each test case starts from
 *  a clean slate. */
export function __resetDeviceBindingForTests(): void {
  cached = null;
}

function base64UrlEncode(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function isValidEncoded(value: string): boolean {
  // base64url of 32 bytes (no padding) is 43 chars; padded variants
  // can be 44 with one `=`. Accept either but reject obvious garbage
  // before sending it to the server.
  if (value.length < 43 || value.length > 44) return false;
  return /^[A-Za-z0-9_-]+=?$/.test(value);
}

function describeError(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
