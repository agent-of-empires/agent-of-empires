// Creates whose outcome is unknown: every response was lost, but the server's detached
// create may still finish. Kept outside the wizard (and in localStorage, across reloads)
// so the request is retried under its original idempotency key until the server answers,
// instead of being dropped and relaunched under a new key.

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
}

const STORAGE_KEY = "aoe-pending-creates";
// Matches the server's failure replay window (`FAILURE_TTL` in create_progress.rs): past
// it a retry could re-run a create that already failed, so the operation is dropped.
export const PENDING_CREATE_MAX_AGE_MS = 24 * 60 * 60 * 1000;
const MAX_RETRY_DELAY_MS = 30_000;

export const PENDING_CREATE_EXPIRED_MESSAGE =
  "Gave up waiting for the server to confirm this session; check the session list before relaunching.";

let handlers: PendingCreateHandlers | null = null;

const isExpired = (p: PendingCreate) => Date.now() - p.since >= PENDING_CREATE_MAX_AGE_MS;
const reconciling = new Set<string>();

/** Well-formed stored entries, expired or not; corrupt ones are dropped. */
function loadValid(): PendingCreate[] {
  try {
    const parsed: unknown = JSON.parse(safeGetItem(STORAGE_KEY) ?? "[]");
    if (!Array.isArray(parsed)) return [];
    return (parsed as PendingCreate[]).filter(
      (p) =>
        typeof p?.body?.idempotency_key === "string" &&
        typeof p.body.path === "string" &&
        typeof p.body.tool === "string" &&
        typeof p.tool === "string" &&
        typeof p.since === "number" &&
        Number.isFinite(p.since),
    );
  } catch {
    return [];
  }
}

function load(): PendingCreate[] {
  return loadValid().filter((p) => !isExpired(p));
}

function save(list: PendingCreate[]): void {
  safeSetItem(STORAGE_KEY, JSON.stringify(list));
}

const isPending = (key: string) => load().some((p) => p.body.idempotency_key === key);

/** Whether `key` is still in storage, aged out or not. */
const isStored = (key: string) => loadValid().some((p) => p.body.idempotency_key === key);

function remove(key: string): void {
  save(loadValid().filter((p) => p.body.idempotency_key !== key));
}

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
      if (isExpired(pending)) {
        const stillOurs = isStored(key);
        remove(key);
        if (stillOurs) handlers?.onFailed(PENDING_CREATE_EXPIRED_MESSAGE, pending);
        return;
      }
      // Adopted by a reopened wizard: it answers for the outcome now.
      if (!isPending(key)) return;
      const result = await createSession(pending.body);
      if (result.network) continue;
      if (!isPending(key)) return;
      remove(key);
      if (result.ok) handlers?.onCreated(result.session, pending);
      else handlers?.onFailed(result.error || "Unknown error", pending);
      return;
    }
  } finally {
    reconciling.delete(key);
  }
}

/** Hand an unresolved create to the app-level owner, which retries it under its key. */
export function trackPendingCreate(pending: PendingCreate): void {
  save([...load().filter((p) => p.body.idempotency_key !== pending.body.idempotency_key), pending]);
  void reconcile(pending);
}

/** The oldest unresolved create, so a reopened wizard retries it rather than a new request. */
export function peekPendingCreate(): PendingCreate | null {
  return load()[0] ?? null;
}

/** Take ownership of `key` away from the app-level owner; the caller now answers for it. */
export function claimPendingCreate(key: string): void {
  remove(key);
}

/** Register the app's outcome handlers and resume creates left unresolved by an earlier page. */
export function startPendingCreates(next: PendingCreateHandlers): void {
  handlers = next;
  // `reconcile` reports one that aged out while no page was open.
  for (const pending of loadValid()) void reconcile(pending);
}
