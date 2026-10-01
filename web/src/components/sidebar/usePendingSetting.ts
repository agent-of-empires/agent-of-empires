import { useRef, useState } from "react";

/** Shows a picked value until the server reports it, reverting if the latest save fails. */
export function usePendingSetting<T>(server: T, save: (next: T) => Promise<boolean>, onError: () => void) {
  const [pending, setPending] = useState<{ value: T } | null>(null);
  const latest = useRef(0);
  // Cleared during render once the poll catches up, so a later server-side change is not masked.
  if (pending && Object.is(pending.value, server)) setPending(null);

  const value = pending ? pending.value : server;
  const set = (next: T) => {
    if (Object.is(next, value)) return;
    const seq = ++latest.current;
    setPending({ value: next });
    void save(next).then((ok) => {
      if (ok || seq !== latest.current) return;
      setPending(null);
      onError();
    });
  };
  return [value, set] as const;
}
