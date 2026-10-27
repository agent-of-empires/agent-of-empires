// REST replay of ACP history: recent-first cold open, forward top-up, and older pages.

import { lastUserBoundaryIndex } from "../../lib/acpHistoryWindow";
import { mergePrependedActivity, type AcpFrame, type TranscriptRow } from "../../lib/acpTypes";
import { toActivityRows, type Action } from "./reducer";

// Re-request this many seqs behind lastSeq; the reducer's seq dedupe makes the overlap idempotent.
const REPLAY_OVERLAP = 50;
const REPLAY_PAGE_SIZE = 1000;
// Bounds the cold-open fetch to fit the dial timeout; past it the window opens at its default size.
const MAX_COLD_OPEN_PAGES = 5;
// Above every real seq, so `before=` returns the newest page.
const TAIL_BEFORE = Number.MAX_SAFE_INTEGER;
// The handshake lands in the first few events of a session.
const HANDSHAKE_PREFIX_SIZE = 50;

/** `acp/replay` response; `rows` is set (and `frames` empty) only for `view=rows`. */
type ReplayPageResponse = {
  frames: AcpFrame[];
  rows?: TranscriptRow[] | null;
  lost: boolean;
  highest_seq: number;
  next_cursor?: number | null;
  has_more?: boolean;
};

type Dispatch = (action: Action) => void;

const getReplay = (sid: string, params: string, signal?: AbortSignal): Promise<Response> =>
  fetch(`/api/sessions/${encodeURIComponent(sid)}/acp/replay?${params}`, { credentials: "same-origin", signal });

// The frames leg feeds control state the daemon doesn't model; `view=rows` feeds the transcript.
const getReplayPair = (sid: string, params: string, signal: AbortSignal): Promise<[Response, Response]> =>
  Promise.all([getReplay(sid, params, signal), getReplay(sid, `${params}&view=rows`, signal)]);

const readRows = async (res: Response): Promise<TranscriptRow[]> =>
  ((await res.json()) as ReplayPageResponse).rows ?? [];

/**
 * Catch up from `lastSeq.current`, updating it on a cold open. A state with a cursor but no loaded history
 * (`oldestSeq` 0) recovers its transcript from the tail, then folds frames forward from its own cursor; the
 * tail is published only once that catch-up succeeds, so a failed one cannot leave a tail the socket snapshot
 * would append older rows after.
 * Resolves to the sequence the replay reached, or null when it failed or was cut short. Errors are swallowed;
 * a later lagged notice retries.
 * Aborting `signal` cancels the requests and stops every later state write, so a stale snapshot cannot land
 * over newer socket content.
 */
export async function fetchReplay(
  sid: string,
  lastSeq: { current: number },
  oldestSeq: number,
  dispatch: Dispatch,
  setHasMoreOlder: (value: boolean) => void,
  signal: AbortSignal,
): Promise<number | null> {
  try {
    if (lastSeq.current === 0) {
      await fetchTail(sid, lastSeq, null, dispatch, setHasMoreOlder, signal);
      return lastSeq.current;
    }
    if (oldestSeq === 0) {
      // Cached control state is cursor-aware and must see each frame once, from its own cursor. Forward rows
      // from that cursor would append older turns after the recovered tail, so rows resume exactly at the tail's end.
      const from = lastSeq.current;
      const catchUp: CatchUp = async (tailEnd, emit) => {
        const reached = await fetchForward(sid, from, emit, signal, "frames");
        if (reached === null || (await fetchForward(sid, tailEnd, emit, signal, "rows", 0)) === null) return null;
        return reached;
      };
      await fetchTail(sid, lastSeq, catchUp, dispatch, setHasMoreOlder, signal);
      return lastSeq.current;
    }
    return await fetchForward(sid, lastSeq.current, dispatch, signal);
  } catch {
    // Best effort.
    return null;
  }
}

/** Runs after the tail loads and before it publishes. Null withholds the tail; else the seq the controls reached. */
type CatchUp = (tailEnd: number, emit: Dispatch) => Promise<number | null>;

async function fetchTail(
  sid: string,
  lastSeq: { current: number },
  catchUp: CatchUp | null,
  dispatch: Dispatch,
  setHasMoreOlder: (value: boolean) => void,
  signal: AbortSignal,
): Promise<void> {
  const applyFrames = catchUp === null;
  const [tailRes, tailRowsRes] = await getReplayPair(sid, `before=${TAIL_BEFORE}&limit=${REPLAY_PAGE_SIZE}`, signal);
  if (!tailRes.ok) return;
  const tail = (await tailRes.json()) as ReplayPageResponse;
  signal.throwIfAborted();
  if (tail.lost) {
    dispatch({ kind: "lagged", skipped: tail.highest_seq });
    return;
  }
  if (!tailRowsRes.ok) return;
  let rows = toActivityRows(await readRows(tailRowsRes), sid);
  let cursor = tail.next_cursor ?? 0;
  let hasMore = tail.has_more ?? false;
  // The handshake lives at the start of the log, which row pages do not carry.
  const needsHandshake = applyFrames && hasMore && cursor > 1;
  // Page back until the last user prompt is loaded, so the first window can open on the whole last turn.
  for (
    let pages = 0;
    pages < MAX_COLD_OPEN_PAGES && hasMore && cursor > 1 && lastUserBoundaryIndex(rows) < 0;
    pages += 1
  ) {
    let page: ReplayPageResponse;
    try {
      const res = await getReplay(sid, `before=${cursor}&limit=${REPLAY_PAGE_SIZE}&view=rows`, signal);
      if (!res.ok) break;
      page = (await res.json()) as ReplayPageResponse;
    } catch {
      break; // An abort is rethrown below; any other failure keeps what is loaded.
    }
    signal.throwIfAborted();
    rows = mergePrependedActivity(toActivityRows(page.rows ?? [], sid), rows);
    const next = page.next_cursor ?? cursor;
    hasMore = (page.has_more ?? false) && next < cursor;
    cursor = next;
  }
  signal.throwIfAborted();
  const caughtUp: Action[] = [];
  const reached = catchUp ? await catchUp(tail.highest_seq, (a) => caughtUp.push(a)) : null;
  if (catchUp && reached === null) {
    signal.throwIfAborted();
    caughtUp.filter((a) => a.kind === "lagged").forEach(dispatch);
    return;
  }
  signal.throwIfAborted();
  dispatch({ kind: "frames", frames: applyFrames ? (tail.frames ?? []) : [], rows, oldestSeq: cursor });
  caughtUp.forEach(dispatch);
  // The dial reads this ref before React commits the dispatches above, so hand the cursor over explicitly.
  if (reached !== null && reached > lastSeq.current) lastSeq.current = reached;
  setHasMoreOlder(hasMore);
  if (applyFrames && tail.highest_seq > lastSeq.current) lastSeq.current = tail.highest_seq;
  if (needsHandshake) {
    const hsRes = await getReplay(sid, `since=0&limit=${HANDSHAKE_PREFIX_SIZE}`, signal);
    if (hsRes.ok) {
      const hs = (await hsRes.json()) as ReplayPageResponse;
      signal.throwIfAborted();
      if ((hs.frames ?? []).length > 0) dispatch({ kind: "handshake", frames: hs.frames });
    }
  }
  dispatch({ kind: "lagged_resolved" });
}

type ForwardLegs = "both" | "frames" | "rows";

/** Returns the seq the catch-up reached, or null when a failed request or a lost log ended it early. */
async function fetchForward(
  sid: string,
  lastSeq: number,
  dispatch: Dispatch,
  signal: AbortSignal,
  legs: ForwardLegs = "both",
  overlap = REPLAY_OVERLAP,
): Promise<number | null> {
  const firstSince = Math.max(0, lastSeq - overlap);
  let cursor = firstSince;
  let target: number | null = null;
  for (;;) {
    const params = `since=${cursor}&limit=${REPLAY_PAGE_SIZE}`;
    // `rows` mode reads only the rows view, which also carries the cursor fields.
    const rowsOnly = legs === "rows";
    const [res, rowsRes] = await Promise.all([
      getReplay(sid, rowsOnly ? `${params}&view=rows` : params, signal),
      legs === "both" ? getReplay(sid, `${params}&view=rows`, signal) : null,
    ]);
    if (!res.ok || (rowsRes && !rowsRes.ok)) return null;
    const data = (await res.json()) as ReplayPageResponse;
    const pageRows = rowsOnly ? (data.rows ?? []) : rowsRes ? await readRows(rowsRes) : [];
    signal.throwIfAborted();
    if (target === null) {
      target = data.highest_seq;
      // The server's log is behind our cursor (e.g. it was reset), so start over.
      if (data.highest_seq < firstSince) dispatch({ kind: "reset" });
    }
    if (data.lost) {
      dispatch({ kind: "lagged", skipped: data.highest_seq });
      return null;
    }
    if ((data.frames ?? []).length > 0 || pageRows.length > 0) {
      dispatch({ kind: "frames", frames: data.frames ?? [], rows: toActivityRows(pageRows, sid) });
    }
    const next = data.next_cursor;
    if (!(data.has_more && next != null && next > cursor && next < target)) break;
    cursor = next;
  }
  dispatch({ kind: "lagged_resolved" });
  return target;
}

/** Fetch the page below `before`. Returns whether more older history remains, or null on failure. */
export async function fetchOlderPage(sid: string, before: number, dispatch: Dispatch): Promise<boolean | null> {
  const res = await getReplay(sid, `before=${before}&limit=${REPLAY_PAGE_SIZE}&view=rows`);
  if (!res.ok) return null;
  const data = (await res.json()) as ReplayPageResponse;
  const rows = toActivityRows(data.rows ?? [], sid);
  if (rows.length > 0) dispatch({ kind: "prepend", rows, oldestSeq: data.next_cursor ?? before });
  return data.has_more ?? false;
}
