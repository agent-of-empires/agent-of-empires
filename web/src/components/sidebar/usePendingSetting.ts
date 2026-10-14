import { useRef, useState } from "react";
import { startedReads, useAppliedRead } from "../../lib/sessionReads";

interface Pending<T> {
  value: T;
  /** The server value when the first pick was made. */
  from: T;
  /** Picks whose saves are in flight, plus the last settled one, which a read in flight may still echo. */
  sent: T[];
  /** Read ordinal at the moment every save had settled; null while any is in flight. */
  settledAt: number | null;
}

/** Shows a picked value until the server reports a change, reverting if the latest save fails. */
export function usePendingSetting<T>(server: T, save: (next: T) => Promise<boolean>, onError: () => void) {
  const [pending, setPending] = useState<Pending<T> | null>(null);
  const latest = useRef(0);
  const inflight = useRef(0);
  const appliedRead = useAppliedRead();
  if (pending) {
    const done =
      pending.settledAt !== null
        ? // Reads that began before the saves settled may be stale; only a later one can override the pick.
          Object.is(server, pending.value) || appliedRead > pending.settledAt
        : // In flight, the server echoing our own picks is not another writer.
          !Object.is(server, pending.from) &&
          (Object.is(server, pending.value) || !pending.sent.some((v) => Object.is(v, server)));
    if (done) setPending(null);
  }

  const value = pending ? pending.value : server;
  const set = (next: T) => {
    if (Object.is(next, value)) return;
    const seq = ++latest.current;
    inflight.current++;
    setPending((p) => ({ value: next, from: p?.from ?? server, sent: [...(p?.sent ?? []), next], settledAt: null }));
    void save(next).then((ok) => {
      inflight.current--;
      if (seq === latest.current && !ok) {
        setPending(null);
        onError();
      } else if (inflight.current === 0) {
        setPending((p) => (p ? { ...p, sent: [p.value], settledAt: startedReads() } : p));
      }
    });
  };
  return [value, set] as const;
}
