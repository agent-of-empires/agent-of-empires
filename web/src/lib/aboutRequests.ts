import type { ServerAbout } from "./api";

export interface AboutRequests {
  /** Ask again; the answer supersedes any request started before it, as after a profile rename. */
  refresh(): Promise<void>;
  /** Recover after a failure: join a request already in flight rather than superseding it. */
  recover(): Promise<void>;
  /** A generation for a request made outside `refresh`, such as the startup tries. */
  begin(): number;
  isCurrent(generation: number): boolean;
}

/**
 * Orders the dashboard's /api/about requests. A newer request discards an older answer, so a stale profile
 * never wins after a rename; a recovery joins the request in flight, so a second Retry can't discard the
 * answer the first one is about to bring.
 */
export function createAboutRequests(
  fetchOnce: () => Promise<ServerAbout | null>,
  on: { start(): void; answer(about: ServerAbout): void; settled(): void },
): AboutRequests {
  let generation = 0;
  let inFlight: Promise<void> | null = null;
  const begin = () => ++generation;
  const isCurrent = (g: number) => g === generation;
  const refresh = () => {
    const mine = begin();
    on.start();
    const request: Promise<void> = fetchOnce()
      .then((about) => {
        if (isCurrent(mine) && about) on.answer(about);
      })
      .finally(() => {
        if (isCurrent(mine)) on.settled();
        if (inFlight === request) inFlight = null;
      });
    inFlight = request;
    return request;
  };
  return { refresh, recover: () => inFlight ?? refresh(), begin, isCurrent };
}
