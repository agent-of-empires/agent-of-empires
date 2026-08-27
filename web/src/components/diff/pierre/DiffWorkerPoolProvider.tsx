import { WorkerPoolContextProvider } from "@pierre/diffs/react";
import { useCallback, useState, type ReactNode } from "react";
import { useShikiTheme } from "../../../hooks/useShikiTheme";

/**
 * Provides a shared off-main-thread highlighter worker pool for the diff
 * renderer, so syntax highlighting of large diffs doesn't block the UI.
 *
 * Keyed by the active Shiki theme so a theme switch re-initializes the pool
 * with the new theme. When `Worker` is unavailable (SSR / jsdom tests) it
 * renders children directly; the diff components then highlight on the main
 * thread (`disableWorkerPool`), preserving correctness.
 *
 * The same bare-children fallback also engages when a worker fails to load.
 * `@pierre/diffs@1.2.12` resolves each worker's init promise only on an
 * `initialize` success message and its own `error` listener merely logs, so a
 * worker that dies at load leaves `initialize()` pending forever while
 * `isWorkingPool()` still reports healthy. The renderer then waits on a pool
 * that will never drain and paints nothing, tab-wide, because the pool is a
 * module-level singleton. See #3362.
 *
 * Swapping the provider for a fragment is what makes the recovery stick:
 * `useFileDiffInstance` captures the pool reference once when the DOM node
 * attaches, so only a subtree remount rebuilds the renderer without a pool.
 * Unmounting also drops the library's instance count to zero, which
 * terminates the poisoned singleton, so the next fresh mount builds a new
 * pool. That is the retry; a retry loop here would just re-race the same
 * broken worker.
 *
 * One known cost: the remount replaces the scroll container that
 * `DiffFileViewer`'s scroll effect captured, so target-line positioning is
 * lost for the file that was open when the worker died. The new container
 * starts at the top, which is that effect's default anyway.
 */
export function DiffWorkerPoolProvider({ children }: { children: ReactNode }) {
  const { theme } = useShikiTheme();
  const [workerFailed, setWorkerFailed] = useState(false);

  const workerFactory = useCallback(() => {
    const worker = new Worker(new URL("@pierre/diffs/worker/worker.js", import.meta.url), { type: "module" });
    // A host-side `error` here means the worker script never ran: the library's
    // worker catches its own per-message failures and reports them as `error`
    // messages instead. So this fires only for the fatal load case.
    worker.addEventListener("error", () => setWorkerFailed(true), { once: true });
    return worker;
  }, []);

  if (typeof Worker === "undefined") {
    return <>{children}</>;
  }

  if (workerFailed) {
    return (
      <>
        <div role="status" className="shrink-0 px-3 py-1 text-[11px] font-mono text-text-dim">
          Highlighting on the main thread; the worker failed to load. Reload to retry.
        </div>
        {children}
      </>
    );
  }

  return (
    <WorkerPoolContextProvider key={theme} poolOptions={{ workerFactory, poolSize: 4 }} highlighterOptions={{ theme }}>
      {children}
    </WorkerPoolContextProvider>
  );
}
