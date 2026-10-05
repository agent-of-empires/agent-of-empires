import { useLayoutEffect, useRef, useState } from "react";
import type { RuntimeCursor, SessionReceipt } from "../../lib/api";

interface Pending<T> {
  identity: string;
  value: T;
  from: T;
  sent: T[];
  receipts: SessionReceipt[] | null;
}

/** Holds the latest pick until every row's canonical mutation result is observed. */
export function usePendingSetting<T>(
  server: T,
  save: (next: T) => Promise<SessionReceipt[] | null>,
  onError: () => void,
  observedById: Record<string, RuntimeCursor>,
  identity: string,
) {
  const [pending, setPending] = useState<Pending<T> | null>(null);
  const latest = useRef(0);
  const lifetime = useRef(identity);
  const mounted = useRef(true);
  const queue = useRef<Promise<void>>(Promise.resolve());
  useLayoutEffect(() => {
    const sequence = latest;
    lifetime.current = identity;
    mounted.current = true;
    queue.current = Promise.resolve();
    return () => {
      mounted.current = false;
      sequence.current++;
    };
  }, [identity]);
  const activePending = pending?.identity === identity ? pending : null;

  if (activePending) {
    const observed =
      activePending.receipts !== null &&
      activePending.receipts.every(({ id, cursor }) => {
        const row = observedById[id];
        return row?.epoch === cursor.epoch && row.revision >= cursor.revision;
      });
    const otherWriter =
      activePending.receipts === null &&
      !Object.is(server, activePending.from) &&
      !activePending.sent.some((value) => Object.is(value, server));
    if (observed || otherWriter) setPending(null);
  }

  const value = activePending ? activePending.value : server;
  const set = (next: T) => {
    if (Object.is(next, value)) return;
    const seq = ++latest.current;
    const owner = identity;
    setPending((previous) => ({
      identity,
      value: next,
      from: previous?.identity === identity ? previous.from : server,
      sent: [...(previous?.identity === identity ? previous.sent : []), next],
      receipts: null,
    }));
    queue.current = queue.current.then(async () => {
      if (!mounted.current || owner !== lifetime.current) return;
      const receipts = await save(next).catch(() => null);
      if (!mounted.current || owner !== lifetime.current || seq !== latest.current) return;
      if (receipts === null) {
        setPending(null);
        onError();
      } else {
        setPending((current) => (current ? { ...current, receipts } : null));
      }
    });
  };
  return [value, set] as const;
}
