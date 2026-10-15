import { WorkerPoolContextProvider } from "@pierre/diffs/react";
import { useCallback, useState, type ReactNode } from "react";
import { useShikiTheme } from "../../../hooks/useShikiTheme";

/**
 * Off-main-thread highlighter pool, keyed by theme; without `Worker` children highlight inline.
 * A worker that fails to load leaves the library's pool pending forever, so children remount
 * without the provider, which also terminates the poisoned singleton.
 */
export function DiffWorkerPoolProvider({ children }: { children: ReactNode }) {
  const { theme } = useShikiTheme();
  const [workerFailed, setWorkerFailed] = useState(false);

  const workerFactory = useCallback(() => {
    const worker = new Worker(new URL("@pierre/diffs/worker/worker.js", import.meta.url), { type: "module" });
    // Per-message failures arrive as messages; a host `error` means the script never loaded.
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
