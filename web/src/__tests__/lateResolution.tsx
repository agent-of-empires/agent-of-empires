import { act, startTransition, useLayoutEffect, useState, type ReactNode } from "react";
import { waitFor } from "@testing-library/react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";

interface LateResolutionCase {
  /** Tree whose async request is left pending. */
  a: ReactNode;
  /** Replacement tree; `bText` appears once it has committed. */
  b: ReactNode;
  bText: string;
  /** Settles A's pending request. */
  resolveStale: () => void;
  /** Runs after the initial mount. */
  afterMount?: (host: HTMLElement) => void;
}

/** Resolve A from B's layout effect before passive cleanup, outside act.
 *  Observe B's commit before flushing the resulting late state updates. */
export async function renderWithLateResolution({ a, b, bText, resolveStale, afterMount }: LateResolutionCase) {
  let setStage!: (s: "a" | "b") => void;
  function Harness() {
    const [stage, set] = useState<"a" | "b">("a");
    useLayoutEffect(() => {
      setStage = set;
    }, [set]);
    useLayoutEffect(() => {
      if (stage === "b") resolveStale();
    }, [stage]);
    return stage === "a" ? a : b;
  }

  const g = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean };
  const prevActEnv = g.IS_REACT_ACT_ENVIRONMENT;
  g.IS_REACT_ACT_ENVIRONMENT = false;
  const host = document.createElement("div");
  document.body.appendChild(host);
  const root = createRoot(host);
  try {
    g.IS_REACT_ACT_ENVIRONMENT = true;
    await act(async () => {
      flushSync(() => root.render(<Harness />));
      afterMount?.(host);
    });
    g.IS_REACT_ACT_ENVIRONMENT = false;
    startTransition(() => setStage("b"));
    await waitFor(
      () => {
        if (!host.textContent?.includes(bText)) throw new Error("Replacement tree has not committed");
      },
      { timeout: 5000 },
    );
    g.IS_REACT_ACT_ENVIRONMENT = true;
    await act(async () => {});
    return { html: host.innerHTML, text: host.textContent ?? "" };
  } finally {
    flushSync(() => root.unmount());
    host.remove();
    g.IS_REACT_ACT_ENVIRONMENT = prevActEnv;
  }
}
