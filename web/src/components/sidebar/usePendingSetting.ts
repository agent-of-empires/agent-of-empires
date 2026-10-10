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
  // flight. Waiting for the picked value instead would mask another writer forever.
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
    setPending((p) =>
      Object.is(next, server) ? null : { value: next, from: p?.from ?? server, sent: [...(p?.sent ?? []), next] },
    );
    void save(next).then((ok) => {
      if (ok || seq !== latest.current) return;
      setPending(null);
      onError();
    });
  };
  return [value, set] as const;
}
