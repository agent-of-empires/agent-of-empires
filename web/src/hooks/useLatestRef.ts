import { useEffect, useRef, type RefObject } from "react";

/** A ref that tracks `value` after each commit, for stable callbacks that need the latest value. */
export function useLatestRef<T>(value: T): RefObject<T> {
  const ref = useRef(value);
  useEffect(() => {
    ref.current = value;
  }, [value]);
  return ref;
}
