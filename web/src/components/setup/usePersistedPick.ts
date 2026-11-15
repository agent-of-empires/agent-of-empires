import { useRef, useState } from "react";

/**
 * A radio selection saved on pick. The parent's `canonical` value updates the rollback baseline and,
 * unless a pick is in flight, the selection; a failed pick rolls back only if no newer pick followed.
 */
export function usePersistedPick<T>(canonical: T, save: (v: T) => Promise<boolean>) {
  const [s, setS] = useState({ canonical, value: canonical, saved: canonical, inFlight: false });
  if (s.canonical !== canonical) {
    setS({ canonical, value: s.inFlight ? s.value : canonical, saved: canonical, inFlight: s.inFlight });
  }
  const latest = useRef(0);
  const pick = async (next: T) => {
    const id = ++latest.current;
    setS((cur) => ({ ...cur, value: next, inFlight: true }));
    const ok = await save(next);
    const last = id === latest.current;
    setS((cur) => {
      const saved = ok ? next : cur.saved;
      return last ? { ...cur, saved, value: saved, inFlight: false } : { ...cur, saved };
    });
  };
  return [s.value, pick] as const;
}
