import { useCallback, useEffect, useRef, useState } from "react";
import type { SessionResponse } from "../lib/types";
import {
  fetchSessions,
  fetchTrashedSessions,
  type RuntimeCursor,
  type SessionMutation,
  type SessionsEnvelope,
} from "../lib/api";
import { setServerDown } from "../lib/connectionState";

const POLL_INTERVAL = 3000;
const POLL_DEADLINE_MS = 15000;
const LOCAL_ORDERING_WINDOW_MS = 4000;

interface SessionState {
  sessions: SessionResponse[];
  observedById: Record<string, RuntimeCursor>;
  epoch: string | null;
}

function mergeSession(session: SessionResponse, previous: SessionResponse): SessionResponse {
  return {
    ...session,
    rate_limit: session.rate_limit === undefined ? previous.rate_limit : session.rate_limit,
    rate_limit_auto_resume: session.rate_limit_auto_resume ?? previous.rate_limit_auto_resume,
    // Omitted when false, and mutation responses never resolve it: keep the polled value.
    structured_view_off: session.structured_view_off ?? previous.structured_view_off,
  };
}

interface PushedStatus {
  status: SessionResponse["status"];
  cursor: RuntimeCursor;
}

/** A pushed status wins over a row observation of the same epoch only while it is newer than that observation. */
const outranks = (held: PushedStatus, observed: RuntimeCursor) =>
  held.cursor.epoch === observed.epoch && held.cursor.revision > observed.revision;

function withPushedStatus(row: SessionResponse, observed: RuntimeCursor, held: PushedStatus | undefined) {
  return held && outranks(held, observed) ? { ...row, status: held.status } : row;
}

export function useSessions() {
  const [state, setState] = useState<SessionState>({ sessions: [], observedById: {}, epoch: null });
  const [workspaceOrdering, setWorkspaceOrdering] = useState<string[]>([]);
  const [error, setError] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [trashedCount, setTrashedCount] = useState(0);
  const [trashEpoch, setTrashEpoch] = useState<string | null>(null);
  // The list omits the trash; once a view asks for it, its rows ride in `state.sessions` and follow every poll.
  const trashWantedRef = useRef(false);
  const trashInFlightRef = useRef<Promise<void> | null>(null);
  const liveIdsRef = useRef(new Set<string>());
  // Rows a trash read found absent, with that read's cursor: a list read older than it must not bring them back.
  const absentRef = useRef(new Map<string, RuntimeCursor>());
  // The newest read or receipt that showed each row, kept after the row leaves the list. A list omission is not a
  // deletion (the row may be trashed), but it does not make an older trash read newer than what was seen.
  const seenRef = useRef(new Map<string, RuntimeCursor>());
  const noteSeen = useCallback((id: string, cursor: RuntimeCursor) => {
    const held = seenRef.current.get(id);
    if (!held || held.epoch !== cursor.epoch || held.revision < cursor.revision) seenRef.current.set(id, cursor);
  }, []);

  const lastLocalOrderingAtRef = useRef<number>(0);
  // GET admission is global; only installed rows attest individual mutation receipts.
  const floorGet = useRef<RuntimeCursor | null>(null);
  const abandonedEpochs = useRef(new Set<string>());
  // Newest pushed status per row. A push is a partial observation: it is compared with each row's own observation,
  // never with the global floor, and one from an epoch not yet installed waits for a read of that epoch.
  const pushedRef = useRef(new Map<string, PushedStatus>());

  const injectSession = useCallback((session: SessionResponse) => {
    setState((prev) =>
      prev.sessions.some((s) => s.id === session.id) ? prev : { ...prev, sessions: [session, ...prev.sessions] },
    );
  }, []);

  const markLocalOrderingUpdate = useCallback(() => {
    lastLocalOrderingAtRef.current = Date.now();
  }, []);

  const applyResult = useCallback(
    (data: SessionsEnvelope | null) => {
      setLoaded(true);
      if (data !== null) {
        setError(false);
        setServerDown(false);
        const cursor = data.cursor;
        const floor = floorGet.current;
        if (abandonedEpochs.current.has(cursor.epoch)) return;
        if (floor?.epoch === cursor.epoch && cursor.revision < floor.revision) return;
        if (floor && floor.epoch !== cursor.epoch) abandonedEpochs.current.add(floor.epoch);
        floorGet.current = cursor;
        const pushed = pushedRef.current;
        const present = new Set(data.sessions.map((s) => s.id));
        const absent = absentRef.current;
        const gone = (id: string) => {
          const seen = absent.get(id);
          return seen?.epoch === cursor.epoch && seen.revision > cursor.revision;
        };
        const sessions = data.sessions
          .filter((s) => !gone(s.id))
          .map((s) => withPushedStatus(s, cursor, pushed.get(s.id)));
        for (const [id, seen] of absent) {
          if (seen.epoch !== cursor.epoch || seen.revision <= cursor.revision) absent.delete(id);
        }
        const rowById = new Map(sessions.map((s) => [s.id, s]));
        for (const [id, seen] of seenRef.current) if (seen.epoch !== cursor.epoch) seenRef.current.delete(id);
        for (const id of present) noteSeen(id, cursor);
        liveIdsRef.current = present;
        for (const [id, held] of pushed) {
          // With the trash wanted, an absent row may be a trashed one, which only a trash read can observe.
          if (trashWantedRef.current && !present.has(id)) continue;
          const waiting = held.cursor.epoch !== cursor.epoch && !abandonedEpochs.current.has(held.cursor.epoch);
          if (!present.has(id) || !(outranks(held, cursor) || waiting)) pushed.delete(id);
        }
        setTrashedCount(data.trashed_count ?? 0);
        setState((prev) => {
          // A row last observed at a newer revision of this epoch than this read (a trash read, for one) is not rolled
          // back, whether this read carries it or omits it. Rows of another epoch are not kept: the next read of this
          // epoch brings them back.
          const sameEpoch = prev.epoch === cursor.epoch;
          const newer = (id: string) => {
            const seen = prev.observedById[id];
            return sameEpoch && seen?.epoch === cursor.epoch && seen.revision > cursor.revision;
          };
          const previous = new Map(prev.sessions.map((s) => [s.id, s]));
          const observedById: Record<string, RuntimeCursor> = {};
          const rows = [...rowById.values()].map((row) => {
            const held = previous.get(row.id);
            if (held && newer(row.id)) {
              observedById[row.id] = prev.observedById[row.id]!;
              return held;
            }
            observedById[row.id] = cursor;
            return row;
          });
          const carried = sameEpoch
            ? prev.sessions.filter(
                (s) => !present.has(s.id) && ((trashWantedRef.current && s.trashed_at != null) || newer(s.id)),
              )
            : [];
          for (const s of carried) {
            const held = prev.observedById[s.id];
            if (held) observedById[s.id] = held;
          }
          return { sessions: [...rows, ...carried], observedById, epoch: cursor.epoch };
        });
        // Ignore server ordering while a local drag's PUT may still be landing.
        if (Date.now() - lastLocalOrderingAtRef.current > LOCAL_ORDERING_WINDOW_MS) {
          setWorkspaceOrdering(data.workspace_ordering);
        }
      } else {
        setError(true);
        setServerDown(true);
      }
    },
    [noteSeen],
  );

  // One trash read at a time, so a slow link still completes one instead of every poll superseding the last.
  const syncTrash = useCallback((): Promise<void> => {
    if (!trashWantedRef.current) return Promise.resolve();
    if (trashInFlightRef.current) return trashInFlightRef.current;
    const request = new AbortController();
    const deadline = setTimeout(() => request.abort(), POLL_DEADLINE_MS);
    const run = (async () => {
      try {
        const data = await fetchTrashedSessions(request.signal);
        if (!data) return;
        const { cursor } = data;
        const floor = floorGet.current;
        // Only a read of the installed epoch may change rows; the next list poll installs a newer epoch and syncs
        // again. It is not held to the newest list revision: live revisions keep advancing, and a slow read would
        // never land. Its rows are fenced one by one below instead.
        if (!floor || floor.epoch !== cursor.epoch || abandonedEpochs.current.has(cursor.epoch)) return;
        const pushed = pushedRef.current;
        setState((prev) => {
          const live = prev.sessions.filter((s) => s.trashed_at == null);
          const present = new Set(live.map((s) => s.id));
          const previous = new Map(prev.sessions.map((s) => [s.id, s]));
          const observedById = { ...prev.observedById };
          const dataIds = new Set(data.sessions.map((s) => s.id));
          const newer = (id: string) => {
            const seen = prev.observedById[id];
            return seen?.epoch === cursor.epoch && seen.revision > cursor.revision;
          };
          // Held trash rows this read omits: kept when observed at a newer revision, else recorded as absent.
          const omitted = prev.sessions.filter((s) => s.trashed_at != null && !present.has(s.id) && !dataIds.has(s.id));
          const kept = omitted.filter((s) => newer(s.id));
          for (const s of omitted) {
            if (newer(s.id)) continue;
            absentRef.current.set(s.id, cursor);
            delete observedById[s.id];
          }
          for (const id of dataIds) {
            const seen = absentRef.current.get(id);
            if (seen && seen.revision <= cursor.revision) absentRef.current.delete(id);
          }
          // A row this read lists but no held row backs is dropped when something newer was seen of it.
          const stale = (id: string) => {
            const seen = seenRef.current.get(id);
            return !previous.has(id) && seen?.epoch === cursor.epoch && seen.revision > cursor.revision;
          };
          const trashed = data.sessions
            .filter((s) => !present.has(s.id) && !stale(s.id))
            .map((s) => {
              // A receipt or read of this row that is no older than the trash read stays.
              const seen = prev.observedById[s.id];
              const held = previous.get(s.id);
              if (held && seen?.epoch === cursor.epoch && seen.revision >= cursor.revision) return held;
              observedById[s.id] = cursor;
              noteSeen(s.id, cursor);
              return withPushedStatus(s, cursor, pushed.get(s.id));
            });
          return { ...prev, sessions: [...live, ...trashed, ...kept], observedById };
        });
        // The list keeps pushes for rows it does not carry while the trash is wanted; this read now covers them.
        const covered = new Set(data.sessions.map((s) => s.id));
        for (const [id, held] of pushed) {
          const waiting = held.cursor.epoch !== cursor.epoch && !abandonedEpochs.current.has(held.cursor.epoch);
          if (liveIdsRef.current.has(id)) continue;
          if (!covered.has(id) || !(outranks(held, cursor) || waiting)) pushed.delete(id);
        }
        setTrashEpoch(cursor.epoch);
      } finally {
        clearTimeout(deadline);
        trashInFlightRef.current = null;
      }
    })();
    trashInFlightRef.current = run;
    return run;
  }, [noteSeen]);

  const loadTrash = useCallback(() => {
    trashWantedRef.current = true;
    void syncTrash();
  }, [syncTrash]);

  // Published after the commit so no row sees the new ordinal beside the previous snapshot.
  useEffect(() => {
    let cancelled = false;
    let generation = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    let controller: AbortController | undefined;
    const scheduleNext = () => {
      if (!cancelled) timer = setTimeout(() => void tick(), POLL_INTERVAL);
    };
    const tick = async () => {
      const mine = ++generation;
      const request = new AbortController();
      controller = request;
      const requestDeadline = setTimeout(() => {
        if (cancelled || mine !== generation) return;
        generation++;
        request.abort();
        applyResult(null);
        scheduleNext();
      }, POLL_DEADLINE_MS);
      deadline = requestDeadline;
      const data = await fetchSessions(request.signal);
      clearTimeout(requestDeadline);
      if (cancelled || mine !== generation) return;
      controller = undefined;
      applyResult(data);
      void syncTrash();
      scheduleNext();
    };
    void tick();
    return () => {
      cancelled = true;
      generation++;
      clearTimeout(timer);
      clearTimeout(deadline);
      controller?.abort();
    };
  }, [applyResult, syncTrash]);

  // Refetches the authoritative list after the stream (re)connected or lost events.
  const refresh = useCallback(() => {
    void fetchSessions().then(applyResult);
    void syncTrash();
  }, [applyResult, syncTrash]);

  // The server sends a move only after publishing a snapshot that includes it, and stamps it with that cursor.
  const applyPushedStatus = useCallback((id: string, status: SessionResponse["status"], cursor: RuntimeCursor) => {
    if (abandonedEpochs.current.has(cursor.epoch)) return;
    const held = pushedRef.current.get(id);
    if (held && held.cursor.epoch === cursor.epoch && held.cursor.revision >= cursor.revision) return;
    pushedRef.current.set(id, { status, cursor });
    setState((prev) => {
      if (prev.epoch !== cursor.epoch) return prev;
      const observed = prev.observedById[id];
      if (observed?.epoch === cursor.epoch && observed.revision >= cursor.revision) return prev;
      return { ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) };
    });
  }, []);

  const setSessionStatus = useCallback((id: string, status: SessionResponse["status"]) => {
    setState((prev) => ({ ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) }));
  }, []);

  const clearDeletingStatus = useCallback((selected: SessionResponse[]) => {
    const originals = new Map(selected.map((session) => [session.id, session]));
    setState((previous) => {
      let sessions: SessionResponse[] | undefined;
      for (const [index, row] of previous.sessions.entries()) {
        const original = originals.get(row.id);
        if (
          row.status === "Deleting" &&
          original &&
          row.profile === original.profile &&
          row.created_at === original.created_at &&
          row.lifecycle_generation === original.lifecycle_generation &&
          row.trashed_at === original.trashed_at &&
          row.trash_lifecycle_id === original.trash_lifecycle_id
        ) {
          sessions ??= previous.sessions.slice();
          sessions[index] = { ...row, status: original.status };
        }
      }
      return sessions ? { ...previous, sessions } : previous;
    });
  }, []);

  const applySession = useCallback((session: SessionResponse) => {
    setState((prev) => {
      const observedById = { ...prev.observedById };
      delete observedById[session.id];
      return {
        ...prev,
        sessions: prev.sessions.map((s) => (s.id === session.id ? mergeSession(session, s) : s)),
        observedById,
      };
    });
  }, []);

  const applySessionMutation = useCallback(
    ({ session, cursor }: SessionMutation) => {
      const floor = floorGet.current;
      if (abandonedEpochs.current.has(cursor.epoch) || (floor && floor.epoch !== cursor.epoch)) return;
      if (!floor || cursor.revision > floor.revision) floorGet.current = cursor;
      noteSeen(session.id, cursor);
      const absentSince = absentRef.current.get(session.id);
      if (absentSince && cursor.revision >= absentSince.revision) absentRef.current.delete(session.id);
      setState((prev) => {
        const observed = prev.observedById[session.id];
        if (observed?.epoch === cursor.epoch && observed.revision >= cursor.revision) return prev;
        // A mutation response cannot resurrect an id absent from the canonical list.
        if (!prev.sessions.some((s) => s.id === session.id)) return prev;
        return {
          sessions: prev.sessions.map((s) =>
            s.id === session.id
              ? withPushedStatus(mergeSession(session, s), cursor, pushedRef.current.get(session.id))
              : s,
          ),
          observedById: { ...prev.observedById, [session.id]: cursor },
          epoch: cursor.epoch,
        };
      });
    },
    [noteSeen],
  );

  return {
    sessions: state.sessions,
    observedById: state.observedById,
    runtimeEpoch: state.epoch,
    workspaceOrdering,
    setWorkspaceOrdering,
    markLocalOrderingUpdate,
    error,
    loaded,
    trashedCount,
    // A restart installs a new epoch whose trash is unread, so a link to a trashed session must wait for it.
    trashLoaded: trashEpoch !== null && trashEpoch === state.epoch,
    loadTrash,
    injectSession,
    setSessionStatus,
    clearDeletingStatus,
    applySession,
    applySessionMutation,
    refresh,
    applyPushedStatus,
  };
}
