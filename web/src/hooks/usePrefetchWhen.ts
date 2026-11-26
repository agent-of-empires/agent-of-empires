import { useEffect } from "react";

/** Calls `load` (a dynamic `import()` thunk) once `when` is true, so the chunk is cached before it is needed. */
export function usePrefetchWhen(when: boolean, load: () => Promise<unknown>): void {
  useEffect(() => {
    // Best effort: a render retries the import and lazyView shows the failure.
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (when) load().catch(() => {});
  }, [when, load]);
}
