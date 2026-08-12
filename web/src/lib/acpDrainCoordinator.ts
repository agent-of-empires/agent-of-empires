// Single-owner coordination for the structured view's queued-prompt drain.
//
// Before #3331 the drain lived entirely inside one `useAcpSession`
// instance: a `drainingRef` guarded it, and the instance only existed
// while its chat was the visible one. Draining a session whose chat is
// unmounted means several hook instances can exist over the life of one
// drain (a headless drainer hands off to the real view when the user
// navigates in), and several tabs can hold a queue for the same session,
// so ownership has to outlive any single component.
//
// Three primitives, deliberately no more:
//
//   - `withDrainLock` makes the drain single-owner. The synchronous
//     `heldLocally` set closes the in-tab window (an effect must not
//     re-enter while the async lock is still being acquired), and the Web
//     Locks API closes the cross-tab one. A tab that dies mid-drain drops
//     its Web Lock automatically, which a localStorage lease cannot do.
//   - `onQueueRetired` / `emitQueueRetired` propagate a completed send to
//     every live hook for that session, and over a BroadcastChannel to
//     every other tab. The lock alone is not enough there: it stops two
//     tabs draining at the same moment, but the loser still holds the
//     delivered rows in its own reducer and cache, so it would re-persist
//     and re-POST them the moment the lock frees.
//   - `armForBackgroundDrain` gates background draining to queues this
//     page parked, or restored ones young enough to still be wanted.
//
// What this deliberately does NOT do: close the duplicate window where a
// POST is accepted server-side but its response is lost. No client-only
// primitive can. That needs an idempotency key honoured by the daemon,
// and lives with the server-side durable queue follow-up.

import { getNewestQueuedAt } from "./acpStateStorage";

/** How old the newest prompt in a restored queue may be and still drain
 *  in the background. Inside the window a reload (mobile tab eviction,
 *  PWA cold start, refresh mid-turn) resumes where it left off; outside
 *  it, the queue waits for the user to open that chat, which is what
 *  happened before #3331. Persisted state lives for `STATE_TTL_MS`
 *  (7 days), far too long to double as authorization to send. */
export const BACKGROUND_DRAIN_MAX_AGE_MS = 60 * 60 * 1000;

/** Sessions whose queue may drain without their chat being open. */
const armed = new Set<string>();
/** Memoised verdict for queues restored from storage, so the arming check
 *  parses a session's persisted entry at most once per page load. A `true`
 *  entry is redundant with `armed` but harmless; `false` is what matters. */
const restoredVerdict = new Map<string, boolean>();

/** Sessions this tab is currently draining. Set synchronously so a
 *  re-render between the effect firing and the Web Lock resolving cannot
 *  start a second drain for the same session. */
const heldLocally = new Set<string>();

type RetiredListener = (ids: string[]) => void;
const retiredListeners = new Map<string, Set<RetiredListener>>();

/** Listeners that want every session's retirements, not one session's.
 *  `useAcpSession` registers one of these to apply the retirement to the
 *  shared state cache, which has to happen whether or not a hook for that
 *  session is currently mounted in this tab. */
type AnyRetiredListener = (sessionId: string, ids: string[]) => void;
const anyRetiredListeners = new Set<AnyRetiredListener>();

const RETIRE_CHANNEL_NAME = "aoe:acp-queue-retired";
type RetiredMessage = { sessionId: string; ids: string[] };

let retireChannel: BroadcastChannel | null | undefined;

/** Lazily open the cross-tab channel. `undefined` means "not tried yet",
 *  `null` means "unavailable here" so we stop retrying. */
function getRetireChannel(): BroadcastChannel | null {
  if (retireChannel !== undefined) return retireChannel;
  if (typeof BroadcastChannel === "undefined") {
    retireChannel = null;
    return null;
  }
  const channel = new BroadcastChannel(RETIRE_CHANNEL_NAME);
  channel.onmessage = (ev: MessageEvent<RetiredMessage>) => {
    const { sessionId, ids } = ev.data ?? {};
    if (typeof sessionId !== "string" || !Array.isArray(ids) || ids.length === 0) return;
    // Apply locally only. Re-broadcasting would ping-pong between tabs.
    fanOutRetired(sessionId, ids);
  };
  retireChannel = channel;
  return channel;
}

function fanOutRetired(sessionId: string, ids: string[]): void {
  for (const cb of anyRetiredListeners) cb(sessionId, ids);
  const set = retiredListeners.get(sessionId);
  if (!set) return;
  for (const cb of set) cb(ids);
}

/** Mark a session's queue eligible for background draining. Called on
 *  every enqueue: a prompt the user parked in this page's lifetime is
 *  unambiguously still wanted. */
export function armForBackgroundDrain(sessionId: string): void {
  armed.add(sessionId);
  restoredVerdict.set(sessionId, true);
}

/** True when this session's queue may drain while its chat is unmounted.
 *  A queue parked in this page's lifetime always qualifies; one restored
 *  from localStorage qualifies only while its newest entry is younger
 *  than `BACKGROUND_DRAIN_MAX_AGE_MS`. */
export function isArmedForBackgroundDrain(sessionId: string): boolean {
  if (armed.has(sessionId)) return true;
  const cached = restoredVerdict.get(sessionId);
  if (cached !== undefined) return cached;
  const newest = getNewestQueuedAt(sessionId);
  const fresh = newest !== null && Date.now() - newest < BACKGROUND_DRAIN_MAX_AGE_MS;
  restoredVerdict.set(sessionId, fresh);
  return fresh;
}

/** True while this tab holds (or is acquiring) the drain for a session. */
export function isDraining(sessionId: string): boolean {
  return heldLocally.has(sessionId);
}

/** Run `fn` as the session's sole drain owner, or skip it entirely when
 *  another owner already holds the drain. Resolves once `fn` settles;
 *  `fn`'s own rejection is surfaced to the caller after the lock is
 *  released. Web Locks is used when available (every current browser
 *  target) and degrades to the in-tab guard alone under jsdom, where the
 *  cross-tab hazard does not exist. */
export async function withDrainLock(sessionId: string, fn: () => Promise<void>): Promise<void> {
  if (heldLocally.has(sessionId)) return;
  heldLocally.add(sessionId);
  try {
    const locks = typeof navigator === "undefined" ? undefined : navigator.locks;
    if (!locks) {
      await fn();
      return;
    }
    // `ifAvailable` hands us a null lock rather than queueing behind the
    // other tab. Queueing would be wrong: by the time we got the lock the
    // items would already be sent, and we would re-POST them.
    await locks.request(`aoe:acp-drain:${sessionId}`, { mode: "exclusive", ifAvailable: true }, async (lock) => {
      if (lock === null) return;
      await fn();
    });
  } finally {
    heldLocally.delete(sessionId);
  }
}

/** Subscribe to "these queued ids were delivered" for a session. Returns
 *  an unsubscribe. Every mounted hook for the session listens, so the
 *  instance that owned the POST need not be the one still mounted when it
 *  resolves. */
export function onQueueRetired(sessionId: string, cb: RetiredListener): () => void {
  getRetireChannel();
  let set = retiredListeners.get(sessionId);
  if (!set) {
    set = new Set();
    retiredListeners.set(sessionId, set);
  }
  set.add(cb);
  return () => {
    const s = retiredListeners.get(sessionId);
    if (!s) return;
    s.delete(cb);
    if (s.size === 0) retiredListeners.delete(sessionId);
  };
}

/** Subscribe to retirements for every session. Returns an unsubscribe.
 *  Used for tab-wide bookkeeping (the shared state cache) that must happen
 *  even when no hook for that session is mounted here. */
export function onAnyQueueRetired(cb: AnyRetiredListener): () => void {
  // Open the channel on subscribe, not on first emit. The tab that loses
  // the drain lock never emits anything, and it is precisely the tab that
  // has to hear about the winner's retirement.
  getRetireChannel();
  anyRetiredListeners.add(cb);
  return () => {
    anyRetiredListeners.delete(cb);
  };
}

/** Announce that `ids` left the queue for good, in this tab and every
 *  other one. Only ever called with a non-empty list: an empty broadcast
 *  would re-run every drain effect and turn a retryable failure into a hot
 *  retry loop. */
export function emitQueueRetired(sessionId: string, ids: string[]): void {
  if (ids.length === 0) return;
  fanOutRetired(sessionId, ids);
  try {
    getRetireChannel()?.postMessage({ sessionId, ids } satisfies RetiredMessage);
  } catch {
    // A closed or unavailable channel must not break the local drain; the
    // other tab falls back to the pre-existing duplicate hazard.
  }
}

/** Drop all coordinator state. Test-only; mirrors `clearAcpCache`. */
export function __resetDrainCoordinator(): void {
  armed.clear();
  restoredVerdict.clear();
  heldLocally.clear();
  retiredListeners.clear();
  anyRetiredListeners.clear();
  retireChannel?.close();
  retireChannel = undefined;
}
