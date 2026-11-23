import { useEffect } from "react";

/** Calls `load` (a dynamic `import()` thunk) once `when` is true, so the chunk is cached before it is needed. */
export function usePrefetchWhen(when: boolean, load: () => unknown): void {
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (when) void load();
  }, [when, load]);
}
