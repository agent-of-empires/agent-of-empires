// @vitest-environment jsdom
//
// A highlighter worker that dies at load used to blank the diff pane forever:
// `@pierre/diffs@1.2.12` never settles `initialize()` in that case, so the
// renderer waits on a pool that will never drain. See #3362. The provider must
// notice the worker error and drop the pool so the renderer falls back to
// main-thread highlighting.
//
// `WorkerPoolContextProvider` is mocked to call the factory the way the real
// pool does, since the real one only builds workers once a renderer triggers
// initialization, which does not happen under jsdom.

import { act, render, screen } from "@testing-library/react";
import { useEffect, useRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DiffWorkerPoolProvider } from "../DiffWorkerPoolProvider";

class FakeWorker {
  static instances: FakeWorker[] = [];
  private listeners: Array<(event: Event) => void> = [];

  constructor() {
    FakeWorker.instances.push(this);
  }

  addEventListener(type: string, cb: (event: Event) => void) {
    if (type === "error") this.listeners.push(cb);
  }
  removeEventListener() {}
  terminate() {}

  /** What the browser does when the worker script fails to load. */
  failToLoad() {
    for (const cb of this.listeners) cb(new Event("error"));
  }
}

vi.mock("@pierre/diffs/react", () => ({
  WorkerPoolContextProvider: ({
    children,
    poolOptions,
  }: {
    children: React.ReactNode;
    poolOptions: { workerFactory: () => Worker };
  }) => {
    const built = useRef(false);
    useEffect(() => {
      if (built.current) return;
      built.current = true;
      poolOptions.workerFactory();
    }, [poolOptions]);
    return <div data-testid="worker-pool">{children}</div>;
  },
}));

afterEach(() => {
  FakeWorker.instances = [];
  vi.unstubAllGlobals();
});

describe("DiffWorkerPoolProvider", () => {
  it("drops the pool and says so when a worker fails to load", () => {
    vi.stubGlobal("Worker", FakeWorker);
    render(
      <DiffWorkerPoolProvider>
        <span>diff body</span>
      </DiffWorkerPoolProvider>,
    );

    expect(screen.getByTestId("worker-pool")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
    expect(FakeWorker.instances).toHaveLength(1);

    act(() => FakeWorker.instances[0].failToLoad());

    // Pool gone, so the renderer remounts without it and highlights on the
    // main thread; the diff itself still renders.
    expect(screen.queryByTestId("worker-pool")).toBeNull();
    expect(screen.getByText("diff body")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toContain("main thread");
  });
});
