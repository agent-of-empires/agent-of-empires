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
  // Ignore earlier picks while the latest save is in flight, but let another writer through.
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
    // Include cancellations so earlier poll echoes cannot displace the latest pick.
    setPending((p) => {
      const prev = p?.sent ?? [];
      return {
        value: next,
        from: p?.from ?? server,
        sent: prev.some((v) => Object.is(v, next)) ? prev : [...prev, next],
      };
    });
    void save(next).then((ok) => {
      if (seq !== latest.current) return;
      if (ok) {
        setPending((p) => (seq === latest.current && p && Object.is(p.value, p.from) ? null : p));
      } else {
        setPending(null);
        onError();
      }
    });
  };
  return [value, set] as const;
}
