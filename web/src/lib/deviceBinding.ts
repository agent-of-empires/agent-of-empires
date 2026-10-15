// Per-browser device-binding secret, the second factor beside the login cookie (`X-Aoe-Device-Binding` header, `aoe-device.<secret>` WS subprotocol). Created on the first authenticated fetch and shared by all tabs.

const STORAGE_KEY = "aoe_device_binding_secret_v1";

/** Matches `BINDING_SECRET_BYTES` in src/server/login.rs. */
const BINDING_SECRET_BYTES = 32;

let cached: string | null = null;

/** Throws when crypto or localStorage is unavailable rather than falling back to a guessable id. */
export function getOrCreateDeviceBindingSecret(): string {
  if (cached !== null) return cached;
  try {
    const existing = window.localStorage.getItem(STORAGE_KEY);
    if (existing && isValidEncoded(existing)) {
      cached = existing;
      return existing;
    }
  } catch {
    // localStorage threw; the write below will throw too and surface it.
  }
  const bytes = new Uint8Array(BINDING_SECRET_BYTES);
  if (typeof crypto === "undefined" || !crypto.getRandomValues) {
    throw new Error("Browser does not expose crypto.getRandomValues; cannot create device binding");
  }
  crypto.getRandomValues(bytes);
  const secret = base64UrlEncode(bytes);
  try {
    // Device binding must hard-fail on quota; callers surface the error
    // to the user rather than silently degrading. Stays on raw setItem.
    // eslint-disable-next-line no-restricted-syntax
    window.localStorage.setItem(STORAGE_KEY, secret);
  } catch (err) {
    throw new Error(`Could not persist device binding secret: ${describeError(err)}`, { cause: err });
  }
  cached = secret;
  return secret;
}

/** Called on logout so the next login gets a fresh binding. */
export function clearDeviceBindingSecret(): void {
  cached = null;
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    // ignore
  }
}

export function __getCachedDeviceBindingSecretForTests(): string | null {
  return cached;
}

export function __resetDeviceBindingForTests(): void {
  cached = null;
}

function base64UrlEncode(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function isValidEncoded(value: string): boolean {
  // base64url of 32 bytes is 43 chars, or 44 padded.
  if (value.length < 43 || value.length > 44) return false;
  return /^[A-Za-z0-9_-]+=?$/.test(value);
}

function describeError(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
