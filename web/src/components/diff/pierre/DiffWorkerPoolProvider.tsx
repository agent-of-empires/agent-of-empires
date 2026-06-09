import { WorkerPoolContextProvider } from "@pierre/diffs/react";
import type { ReactNode } from "react";
import { useShikiTheme } from "../../../hooks/useShikiTheme";

/**
 * Creates a Pierre highlighter worker. Vite bundles the worker entry from
 * `@pierre/diffs/worker/worker.js` when referenced via `new URL(..., import.meta.url)`.
 */
function workerFactory(): Worker {
  return new Worker(new URL("@pierre/diffs/worker/worker.js", import.meta.url), { type: "module" });
}

/**
 * Provides a shared off-main-thread highlighter worker pool for the diff
 * renderer, so syntax highlighting of large diffs doesn't block the UI.
 *
 * Keyed by the active Shiki theme so a theme switch re-initializes the pool
 * with the new theme. When `Worker` is unavailable (SSR / jsdom tests) it
 * renders children directly; the diff components then highlight on the main
 * thread (`disableWorkerPool`), preserving correctness.
 */
export function DiffWorkerPoolProvider({ children }: { children: ReactNode }) {
  const { theme } = useShikiTheme();

  if (typeof Worker === "undefined") {
    return <>{children}</>;
  }

  return (
    <WorkerPoolContextProvider key={theme} poolOptions={{ workerFactory, poolSize: 4 }} highlighterOptions={{ theme }}>
      {children}
    </WorkerPoolContextProvider>
  );
}
