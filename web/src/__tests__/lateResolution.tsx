/// <reference lib="es2024.promise" />

import { Profiler, startTransition, useLayoutEffect, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { flushSync } from "react-dom";

interface LateResolutionCase {
  a: ReactNode;
  b: ReactNode;
  /** Signals the consumer's actual pending request, not promise creation. */
  requestStarted: Promise<void>;
  resolveStale: () => void;
  /** Runs after A mounts and before B is requested, for example to expand a card. */
  afterMount?: (host: HTMLElement) => void;
}

/** Settle A from B's layout, outside act, then await its result-state commit.
 * These trees must have no unrelated updates after B. */
export async function renderWithLateResolution({ a, b, requestStarted, resolveStale, afterMount }: LateResolutionCase) {
  let setStage!: (stage: "a" | "b") => void;
  let replacementInLayout = false;
  let replacementCommitted = false;
  const replacement = Promise.withResolvers<void>();
  const staleUpdate = Promise.withResolvers<void>();
  const timedOut = Promise.withResolvers<never>();
  function Harness() {
    const [stage, set] = useState<"a" | "b">("a");
    useLayoutEffect(() => {
      setStage = set;
    }, [set]);
    useLayoutEffect(() => {
      if (stage === "b") {
        replacementInLayout = true;
        resolveStale();
      }
    }, [stage]);
    return stage === "a" ? a : b;
  }

  const host = document.createElement("div");
  const g = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean };
  const prevActEnv = g.IS_REACT_ACT_ENVIRONMENT;
  let root: Root | undefined;
  let watchdog: number | undefined;
  try {
    g.IS_REACT_ACT_ENVIRONMENT = false;
    document.body.appendChild(host);
    const mountedRoot = createRoot(host);
    root = mountedRoot;
    watchdog = window.setTimeout(() => timedOut.reject(new Error("Late-resolution commit did not complete")), 2000);
    flushSync(() =>
      mountedRoot.render(
        <Profiler
          id="late-resolution"
          onRender={() => {
            if (!replacementInLayout) return;
            if (!replacementCommitted) {
              replacementCommitted = true;
              replacement.resolve();
            } else {
              staleUpdate.resolve();
            }
          }}
        >
          <Harness />
        </Profiler>,
      ),
    );
    afterMount?.(host);
    await Promise.race([requestStarted, timedOut.promise]);
    startTransition(() => setStage("b"));
    await Promise.race([replacement.promise, timedOut.promise]);
    await Promise.race([staleUpdate.promise, timedOut.promise]);
    return { html: host.innerHTML, text: host.textContent ?? "" };
  } finally {
    window.clearTimeout(watchdog);
    try {
      if (root) {
        const mountedRoot = root;
        flushSync(() => mountedRoot.unmount());
      }
    } finally {
      try {
        host.remove();
      } finally {
        g.IS_REACT_ACT_ENVIRONMENT = prevActEnv;
      }
    }
  }
}
