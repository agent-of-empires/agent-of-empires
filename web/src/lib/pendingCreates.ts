// Keyed creates whose outcome is not yet known. The record lives from the first request
// until a definite server answer, in memory and mirrored to localStorage so a reload keeps
// it, and the request is only ever retried under its original idempotency key. A wizard
// working on a create claims it; unclaimed creates are retried here, past the wizard's
// unmount, until the server answers.

import { createSession } from "./api";
import { safeGetItem, safeSetItem } from "./safeStorage";
import type { CreateSessionRequest, SessionResponse } from "./types";

export interface PendingCreate {
  body: CreateSessionRequest & { idempotency_key: string };
  tool: string;
  since: number;
}

export interface PendingCreateHandlers {
  onCreated: (session: SessionResponse | undefined, pending: PendingCreate) => void;
  onFailed: (message: string, pending: PendingCreate) => void;
  /** Storage refused the record: it is still retried, but a reload would lose it. */
  onUnsaved?: (pending: PendingCreate) => void;
}

const STORAGE_KEY = "aoe-pending-creates";
// Matches the server's failure replay window (`FAILURE_TTL` in create_progress.rs): past
// it a retry could re-run a create that already failed, so the operation is dropped.
export const PENDING_CREATE_MAX_AGE_MS = 24 * 60 * 60 * 1000;
const MAX_RETRY_DELAY_MS = 30_000;

export const PENDING_CREATE_EXPIRED_MESSAGE =
  "Gave up waiting for the server to confirm this session; check the session list before relaunching.";

let handlers: PendingCreateHandlers | null = null;
// The source of truth; storage is a best-effort mirror of it.
let registry: Map<string, PendingCreate> | null = null;
// Keys a wizard is working on; the owner leaves them alone until released.
const claimed = new Set<string>();
const reconciling = new Set<string>();
const reportedUnsaved = new Set<string>();

/** Past the server's replay window, a retry could re-run a create that already failed. */
export const isPendingCreateExpired = (since: number) => Date.now() - since >= PENDING_CREATE_MAX_AGE_MS;
const isExpired = (p: PendingCreate) => isPendingCreateExpired(p.since);

function isWellFormed(p: unknown): p is PendingCreate {
  const c = p as PendingCreate | null;
  return (
    typeof c?.body?.idempotency_key === "string" &&
    typeof c.body.path === "string" &&
    typeof c.body.tool === "string" &&
    typeof c.tool === "string" &&
    typeof c.since === "number" &&
    Number.isFinite(c.since)
  );
}

function entries(): Map<string, PendingCreate> {
  if (registry) return registry;
  registry = new Map();
  try {
    const parsed: unknown = JSON.parse(safeGetItem(STORAGE_KEY) ?? "[]");
    if (Array.isArray(parsed)) {
      for (const p of parsed) if (isWellFormed(p)) registry.set(p.body.idempotency_key, p);
    }
  } catch {
    // A corrupt mirror is dropped; nothing else can recover it.
  }
  return registry;
}

function persist(): void {
  const list = [...entries().values()];
  if (safeSetItem(STORAGE_KEY, JSON.stringify(list))) return;
  for (const p of list) {
    if (reportedUnsaved.has(p.body.idempotency_key)) continue;
    reportedUnsaved.add(p.body.idempotency_key);
    handlers?.onUnsaved?.(p);
  }
}

const isOwned = (key: string) => entries().has(key) && !claimed.has(key);

function waitUntilReachable(): Promise<void> {
  const ready = () =>
    (typeof navigator === "undefined" || navigator.onLine !== false) &&
    (typeof document === "undefined" || document.visibilityState === "visible");
  if (ready()) return Promise.resolve();
  return new Promise((resolve) => {
    const check = () => {
      if (!ready()) return;
      window.removeEventListener("online", check);
      document.removeEventListener("visibilitychange", check);
      resolve();
    };
    window.addEventListener("online", check);
    document.addEventListener("visibilitychange", check);
  });
}

async function reconcile(pending: PendingCreate): Promise<void> {
  const key = pending.body.idempotency_key;
  if (reconciling.has(key)) return;
  reconciling.add(key);
  try {
    for (let attempt = 0; ; attempt++) {
      if (attempt > 0) {
        await waitUntilReachable();
        await new Promise((r) => setTimeout(r, Math.min(1000 * attempt, MAX_RETRY_DELAY_MS)));
      }
      // Resolved, or claimed by a wizard, which answers for it until it releases it.
      if (!isOwned(key)) return;
      if (isExpired(pending)) {
        resolvePendingCreate(key);
        handlers?.onFailed(PENDING_CREATE_EXPIRED_MESSAGE, pending);
        return;
      }
      const result = await createSession(pending.body);
      if (result.network) continue;
      if (!isOwned(key)) return;
      resolvePendingCreate(key);
      if (result.ok) handlers?.onCreated(result.session, pending);
      else handlers?.onFailed(result.error || "Unknown error", pending);
      return;
    }
  } finally {
    reconciling.delete(key);
  }
}

/** Record a create before its request goes out; `claimed` when a wizard is sending it. */
export function registerPendingCreate(pending: PendingCreate, { claimed: isClaimed = false } = {}): void {
  const key = pending.body.idempotency_key;
  entries().set(key, pending);
  if (isClaimed) claimed.add(key);
  else claimed.delete(key);
  persist();
  if (!isClaimed) void reconcile(pending);
}

/** The oldest create nobody is working on, so a reopened wizard retries it rather than a new request. */
export function peekPendingCreate(): PendingCreate | null {
  for (const p of entries().values()) {
    if (!claimed.has(p.body.idempotency_key) && !isExpired(p)) return p;
  }
  return null;
}

/** A wizard takes over retrying `key`; the record stays until a definite answer. */
export function claimPendingCreate(key: string): void {
  claimed.add(key);
}

/** The wizard stops working on `key`; the owner resumes it if it is still unresolved. */
export function releasePendingCreate(key: string): void {
  claimed.delete(key);
  const pending = entries().get(key);
  if (pending) void reconcile(pending);
}

/** The server answered `key`: forget it. */
export function resolvePendingCreate(key: string): void {
  claimed.delete(key);
  reportedUnsaved.delete(key);
  if (entries().delete(key)) persist();
}

/** Register the app's outcome handlers and resume creates left unresolved by an earlier page. */
export function startPendingCreates(next: PendingCreateHandlers): void {
  handlers = next;
  // `reconcile` reports one that aged out while no page was open.
  for (const pending of entries().values()) void reconcile(pending);
}
