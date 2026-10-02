import { useRef, useState } from "react";

interface Pending<T> {
  value: T;
  /** The server value when the first pick was made. */
  from: T;
  sent: T[];
}

/** Shows a picked value until the server reports a change, reverting if the latest save fails. */
export function usePendingSetting<T>(server: T, save: (next: T) => Promise<boolean>, onError: () => void) {
  const [pending, setPending] = useState<Pending<T> | null>(null);
  const latest = useRef(0);
  // Cleared during render once the server moves, unless it moved to an earlier pick of ours while a later one is in
  // flight. Waiting for the picked value instead would mask another writer forever. A cancelled pick (`value` back to
  // `from`) is deliberately left holding: it only reads as `server`, and dropping it would let the in-flight earlier
  // pick's echo through as if another writer had made it.
  if (
    pending &&
    !Object.is(server, pending.from) &&
    (Object.is(server, pending.value) || !pending.sent.some((v) => Object.is(v, server)))
  ) {
    setPending(null);
  }

  const value = pending ? pending.value : server;
  const set = (next: T) => {
    if (Object.is(next, value)) return;
    const seq = ++latest.current;
    // Recorded even when `next` is the server value: that PATCH is in flight too, and `sent` is what lets the poll
    // reporting an earlier pick be recognised as our own echo rather than another writer's change.
    setPending((p) => {
      const prev = p?.sent ?? [];
      return {
        value: next,
        from: p?.from ?? server,
        sent: prev.some((v) => Object.is(v, next)) ? prev : [...prev, next],
      };
    });
    void save(next).then((ok) => {
      if (ok || seq !== latest.current) return;
      setPending(null);
      onError();
    });
  };
  return [value, set] as const;
}
