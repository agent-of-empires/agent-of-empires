// REST replay of ACP history: recent-first cold open, forward top-up, and older pages.

import type { AcpFrame, TranscriptRow } from "../../lib/acpTypes";
import { toActivityRows, type Action } from "./reducer";

// Re-request this many seqs behind lastSeq; the reducer's seq dedupe makes the overlap idempotent.
const REPLAY_OVERLAP = 50;
const REPLAY_PAGE_SIZE = 1000;
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
 * Catch up from `lastSeq.current`, updating it on a cold open. Errors are swallowed; a later lagged notice retries.
 * Aborting `signal` cancels the requests and stops every later state write, so a stale snapshot cannot land
 * over newer socket content.
 */
export async function fetchReplay(
  sid: string,
  lastSeq: { current: number },
  dispatch: Dispatch,
  setHasMoreOlder: (value: boolean) => void,
  signal: AbortSignal,
): Promise<void> {
  try {
    if (lastSeq.current === 0) await fetchTail(sid, lastSeq, dispatch, setHasMoreOlder, signal);
    else await fetchForward(sid, lastSeq.current, dispatch, signal);
  } catch {
    // Best effort.
  }
}

async function fetchTail(
  sid: string,
  lastSeq: { current: number },
  dispatch: Dispatch,
  setHasMoreOlder: (value: boolean) => void,
  signal: AbortSignal,
): Promise<void> {
  const [tailRes, tailRowsRes] = await getReplayPair(sid, `before=${TAIL_BEFORE}&limit=${REPLAY_PAGE_SIZE}`, signal);
  if (!tailRes.ok) return;
  const tail = (await tailRes.json()) as ReplayPageResponse;
  signal.throwIfAborted();
  if (tail.lost) {
    dispatch({ kind: "lagged", skipped: tail.highest_seq });
    return;
  }
  if (!tailRowsRes.ok) return;
  const rows = toActivityRows(await readRows(tailRowsRes), sid);
  signal.throwIfAborted();
  dispatch({ kind: "frames", frames: tail.frames ?? [], rows, oldestSeq: tail.next_cursor ?? 0 });
  setHasMoreOlder(tail.has_more ?? false);
  if (tail.highest_seq > lastSeq.current) lastSeq.current = tail.highest_seq;
  if ((tail.has_more ?? false) && (tail.next_cursor ?? 0) > 1) {
    const hsRes = await getReplay(sid, `since=0&limit=${HANDSHAKE_PREFIX_SIZE}`, signal);
    if (hsRes.ok) {
      const hs = (await hsRes.json()) as ReplayPageResponse;
      signal.throwIfAborted();
      if ((hs.frames ?? []).length > 0) dispatch({ kind: "handshake", frames: hs.frames });
    }
  }
  dispatch({ kind: "lagged_resolved" });
}

async function fetchForward(sid: string, lastSeq: number, dispatch: Dispatch, signal: AbortSignal): Promise<void> {
  const firstSince = Math.max(0, lastSeq - REPLAY_OVERLAP);
  let cursor = firstSince;
  let target: number | null = null;
  for (;;) {
    const [res, rowsRes] = await getReplayPair(sid, `since=${cursor}&limit=${REPLAY_PAGE_SIZE}`, signal);
    if (!res.ok || !rowsRes.ok) return;
    const data = (await res.json()) as ReplayPageResponse;
    const pageRows = await readRows(rowsRes);
    signal.throwIfAborted();
    if (target === null) {
      target = data.highest_seq;
      // The server's log is behind our cursor (e.g. it was reset), so start over.
      if (data.highest_seq < firstSince) dispatch({ kind: "reset" });
    }
    if (data.lost) {
      dispatch({ kind: "lagged", skipped: data.highest_seq });
      return;
    }
    if (data.frames.length > 0 || pageRows.length > 0) {
      dispatch({ kind: "frames", frames: data.frames, rows: toActivityRows(pageRows, sid) });
    }
    const next = data.next_cursor;
    if (!(data.has_more && next != null && next > cursor && next < target)) break;
    cursor = next;
  }
  dispatch({ kind: "lagged_resolved" });
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
