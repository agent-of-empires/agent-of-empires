import { startTransition, useLayoutEffect, useState, type ReactNode } from "react";
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
  /** Runs after A mounts, before its effect starts the request. */
  afterMount?: (host: HTMLElement) => void;
}

/** Commits `b` in a transition and settles A's request from `b`'s layout
 *  effect, before A's passive cleanup runs. Uses a raw root outside the act
 *  environment, since act flushes that cleanup first and hides the race.
 *  Returns the host HTML after the late write has had a chance to land. */
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
  const macrotask = () => new Promise<void>((r) => setTimeout(r, 0));
  try {
    flushSync(() => root.render(<Harness />));
    afterMount?.(host);
    await macrotask();

    startTransition(() => setStage("b"));
    for (let i = 0; i < 20 && !host.innerHTML.includes(bText); i++) {
      await Promise.resolve();
    }
    await macrotask();
    await macrotask();
    return { html: host.innerHTML, text: host.textContent ?? "" };
  } finally {
    flushSync(() => root.unmount());
    host.remove();
    g.IS_REACT_ACT_ENVIRONMENT = prevActEnv;
  }
}
