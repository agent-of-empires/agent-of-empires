// @vitest-environment jsdom

import { act, render } from "@testing-library/react";
import { useCallback, useLayoutEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useTranscriptScroll } from "./useTranscriptScroll";
import { saveScrollState } from "../../lib/acpScrollState";
import type { ActivityRow } from "../../lib/acpTypes";
const originalMatchMedia = Object.getOwnPropertyDescriptor(window, "matchMedia");

class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  readonly targets = new Set<Element>();

  constructor(private readonly callback: ResizeObserverCallback) {
    FakeResizeObserver.instances.push(this);
  }

  observe(target: Element): void {
    this.targets.add(target);
  }

  disconnect(): void {}

  fire(): void {
    this.callback([], this as unknown as ResizeObserver);
  }
}

type Metrics = {
  clientHeight: number;
  scrollHeight: number;
  scrollTop: number;
  cardPageTop: number;
};

type HarnessProps = {
  metrics: Metrics;
  sessionId: string;
  folded: boolean;
  closed: boolean;
  streamed: boolean;
  historyRows?: readonly ActivityRow[];
  loadEarlier?: () => void;
};

function box(top: number, bottom: number): DOMRect {
  return {
    x: 0,
    y: top,
    width: 400,
    height: bottom - top,
    top,
    right: 400,
    bottom,
    left: 0,
    toJSON: () => ({}),
  };
}

function Harness({ metrics, sessionId, folded, closed, streamed, historyRows, loadEarlier }: HarnessProps) {
  const { viewportRef, belowViewportRef, messagesContentRef, requestEarlierHistory } = useTranscriptScroll({
    sessionId,
    canLoadEarlierHistory: Boolean(loadEarlier),
    loadEarlierHistory: loadEarlier ?? (() => {}),
    loadingEarlierHistory: false,
    historyRows,
    composerCollapsed: false,
    promptSeq: 0,
    hasEverOpened: true,
    localInflight: false,
  });
  useLayoutEffect(() => {
    const node = viewportRef.current;
    if (!node) return;
    Object.defineProperties(node, {
      clientHeight: { configurable: true, get: () => metrics.clientHeight },
      scrollHeight: { configurable: true, get: () => metrics.scrollHeight },
      scrollTop: {
        configurable: true,
        get: () => metrics.scrollTop,
        set: (value: number) => {
          metrics.scrollTop = Math.max(0, Math.min(value, metrics.scrollHeight - metrics.clientHeight));
        },
      },
      scrollTo: {
        configurable: true,
        value: ({ top }: ScrollToOptions) => {
          node.scrollTop = top ?? 0;
        },
      },
    });
    node.getBoundingClientRect = () => box(100, 100 + metrics.clientHeight);
  }, [metrics, viewportRef]);
  const setCard = useCallback(
    (node: HTMLElement | null) => {
      if (!node) return;
      node.getBoundingClientRect = () => {
        const top = 100 + metrics.cardPageTop - metrics.scrollTop;
        return box(top, top + 20);
      };
    },
    [metrics],
  );

  const card = closed ? null : folded ? (
    <section ref={setCard} data-tool-id="tool-1">
      tool output
    </section>
  ) : (
    <article ref={setCard} data-tool-id="tool-1">
      tool output
    </article>
  );

  return (
    <div ref={viewportRef} data-testid="viewport">
      <button type="button" onClick={requestEarlierHistory}>
        Earlier
      </button>
      <div ref={messagesContentRef} data-testid="content">
        {card}
        {streamed && <p>Follow-up text</p>}
      </div>
      <div ref={belowViewportRef} />
    </div>
  );
}

function contentObserver(content: Element): FakeResizeObserver {
  const observer = FakeResizeObserver.instances.find((instance) => instance.targets.has(content));
  if (!observer) throw new Error("content ResizeObserver was not installed");
  return observer;
}

function offset(element: Element, viewport: Element): number {
  return element.getBoundingClientRect().top - viewport.getBoundingClientRect().top;
}

beforeEach(() => {
  FakeResizeObserver.instances = [];
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  if (originalMatchMedia) Object.defineProperty(window, "matchMedia", originalMatchMedia);
  else Reflect.deleteProperty(window, "matchMedia");
});
describe("useTranscriptScroll history anchors", () => {
  it("keeps the explicit request anchor when preference growth overlaps its pending load", () => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = "history-overlap";
    saveScrollState(sessionId, { stuck: false, top: 550 });
    const recent: ActivityRow = { id: "recent", kind: "message", text: "recent" };
    const older: ActivityRow = { id: "older", kind: "message", text: "older" };
    const loadEarlier = vi.fn();
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true, loadEarlier };
    const view = render(<Harness {...props} historyRows={[recent]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));
    act(() => view.getByRole("button", { name: "Earlier" }).click());
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    metrics.scrollHeight += 100;
    act(() => viewport.dispatchEvent(new Event("scroll")));
    metrics.scrollHeight += 300;
    view.rerender(<Harness {...props} historyRows={[older, recent]} />);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(950);
  });

  it("uses settled streamed growth as the baseline for a later preference prepend", () => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = "history-stream-before-prepend";
    saveScrollState(sessionId, { stuck: false, top: 550 });
    const recent: ActivityRow = { id: "recent", kind: "message", text: "recent" };
    const older: ActivityRow = { id: "older", kind: "message", text: "older" };
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true };
    const view = render(<Harness {...props} historyRows={[recent]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));
    metrics.scrollHeight = 1300;
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(550);
    metrics.scrollHeight = 1600;
    view.rerender(<Harness {...props} historyRows={[older, recent]} />);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(850);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(850);
  });

  it("expires an invisible prepend anchor before later streamed text grows", () => {
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => frames.push(callback));
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = "history-invisible-prepend";
    saveScrollState(sessionId, { stuck: false, top: 550 });
    const recent: ActivityRow = { id: "recent", kind: "message", text: "recent" };
    const older: ActivityRow = { id: "older", kind: "message", text: "hidden before clear" };
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true };
    const view = render(<Harness {...props} historyRows={[recent]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));
    view.rerender(<Harness {...props} historyRows={[older, recent]} />);
    act(() => {
      while (frames.length > 0) frames.shift()!(0);
    });
    metrics.scrollHeight += 300;
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(550);
  });
});

describe("useTranscriptScroll fold corrections", () => {
  it.each([false, true])("preserves text-only history prepends (pinned=%s)", (pinned) => {
    const metrics: Metrics = {
      clientHeight: 100,
      scrollHeight: 1200,
      scrollTop: pinned ? 1100 : 550,
      cardPageTop: 630,
    };
    const sessionId = `history-prepend-${pinned}`;
    saveScrollState(sessionId, { stuck: pinned, top: metrics.scrollTop });
    const recent: ActivityRow = { id: "recent", kind: "message", text: "recent" };
    const older: ActivityRow = { id: "older", kind: "message", text: "older" };
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true };
    const view = render(<Harness {...props} historyRows={[recent]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));
    metrics.scrollHeight += 300;
    view.rerender(<Harness {...props} historyRows={[older, recent]} />);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(pinned ? 1400 : 850);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(pinned ? 1400 : 850);
  });

  it.each(["append", "trim", "replace", "session"])("does not compensate a history %s as a prepend", (change) => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = `history-non-prepend-${change}`;
    saveScrollState(sessionId, { stuck: false, top: 550 });
    saveScrollState(`${sessionId}-next`, { stuck: false, top: 550 });
    const first: ActivityRow = { id: "first", kind: "message", text: "first" };
    const last: ActivityRow = { id: "last", kind: "message", text: "last" };
    const newRow: ActivityRow = { id: "new", kind: "message", text: "new" };
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true };
    const view = render(<Harness {...props} historyRows={[first, last]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    act(() => viewport.dispatchEvent(new Event("scroll")));
    metrics.scrollHeight += 300;
    const rows = change === "append" ? [first, last, newRow] : change === "trim" ? [last] : [newRow];
    view.rerender(
      <Harness {...props} sessionId={change === "session" ? `${sessionId}-next` : sessionId} historyRows={rows} />,
    );
    act(() => contentObserver(view.getByTestId("content")).fire());
    expect(viewport.scrollTop).toBe(550);
  });

  it("loads earlier only on reader scrolling, not initial or delayed content layout", () => {
    vi.spyOn(performance, "now").mockReturnValue(1000);
    const loadEarlier = vi.fn();
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 0, cardPageTop: 630 };
    const sessionId = "history-layout-no-autoload";
    saveScrollState(sessionId, { stuck: false, top: 0 });
    const view = render(
      <Harness
        metrics={metrics}
        sessionId={sessionId}
        folded={false}
        closed={true}
        streamed
        loadEarlier={loadEarlier}
      />,
    );
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    act(() => contentObserver(view.getByTestId("content")).fire());
    expect(loadEarlier).not.toHaveBeenCalled();
    metrics.scrollHeight += 300;
    act(() => contentObserver(view.getByTestId("content")).fire());
    act(() => viewport.dispatchEvent(new Event("scroll")));
    expect(loadEarlier).not.toHaveBeenCalled();
    act(() => {
      viewport.scrollTop = 300;
      viewport.dispatchEvent(new Event("scroll"));
      viewport.scrollTop = 0;
      viewport.dispatchEvent(new Event("scroll"));
    });
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    vi.restoreAllMocks();
  });

  it("rearms after prepend compensation so Home loads again without layout loading", () => {
    const now = vi.spyOn(performance, "now").mockReturnValue(1000);
    const loadEarlier = vi.fn();
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = "history-compensation-rearm";
    saveScrollState(sessionId, { stuck: false, top: 550 });
    const recent: ActivityRow = { id: "recent", kind: "message", text: "recent" };
    const older: ActivityRow = { id: "older", kind: "message", text: "older" };
    const props = { metrics, sessionId, folded: false, closed: true, streamed: true, loadEarlier };
    const view = render(<Harness {...props} historyRows={[recent]} />);
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));
    act(() => {
      viewport.scrollTop = 0;
      viewport.dispatchEvent(new Event("scroll"));
    });
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    metrics.scrollHeight += 300;
    view.rerender(<Harness {...props} historyRows={[older, recent]} />);
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(300);
    act(() => viewport.dispatchEvent(new Event("scroll")));
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    now.mockReturnValue(2000);
    act(() => contentObserver(content).fire());
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    act(() => {
      viewport.dispatchEvent(new KeyboardEvent("keydown", { key: "Home" }));
      viewport.scrollTop = 0;
      viewport.dispatchEvent(new Event("scroll"));
    });
    expect(loadEarlier).toHaveBeenCalledTimes(2);
  });

  it("preserves escape scrolling coalesced with the observer's bottom-following scroll", () => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 1100, cardPageTop: 1150 };
    const sessionId = "streaming-reader-escape";
    saveScrollState(sessionId, { stuck: true, top: metrics.scrollTop });
    const view = render(
      <Harness metrics={metrics} sessionId={sessionId} folded={false} closed={false} streamed={false} />,
    );
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));

    metrics.scrollHeight = 1400;
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(1300);
    act(() => {
      viewport.scrollTop -= 160;
      viewport.dispatchEvent(new Event("scroll"));
    });
    expect(viewport.scrollTop).toBe(1140);

    metrics.scrollHeight = 1600;
    act(() => contentObserver(content).fire());
    expect(viewport.scrollTop).toBe(1140);
  });

  it("keeps pre-fold anchors across a gesture scroll and preserves later keyboard movement", () => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 550, cardPageTop: 630 };
    const sessionId = "fold-anchor-reader-movement";
    saveScrollState(sessionId, { stuck: false, top: metrics.scrollTop });
    const view = render(
      <Harness metrics={metrics} sessionId={sessionId} folded={false} closed={false} streamed={false} />,
    );
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");

    act(() => {
      viewport.dispatchEvent(new Event("wheel"));
      viewport.scrollTop = 600;
      viewport.dispatchEvent(new Event("scroll"));
    });
    const cardBeforeFold = view.container.querySelector('[data-tool-id="tool-1"]')!;
    const beforeFold = offset(cardBeforeFold, viewport);

    metrics.scrollHeight = 1150;
    metrics.cardPageTop = 600;
    view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={false} streamed={false} />);
    const cardAfterFold = view.container.querySelector('[data-tool-id="tool-1"]')!;
    act(() => viewport.dispatchEvent(new Event("scroll")));
    act(() => contentObserver(content).fire());
    expect(offset(cardAfterFold, viewport)).toBe(beforeFold);

    act(() => {
      viewport.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
      viewport.scrollTop -= 40;
      viewport.dispatchEvent(new Event("scroll"));
    });
    const keyboardScrollTop = viewport.scrollTop;
    const afterKeyboard = offset(cardAfterFold, viewport);

    metrics.scrollHeight = 1250;
    act(() => {
      view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={false} streamed={true} />);
      contentObserver(content).fire();
    });
    expect(viewport.scrollTop).toBe(keyboardScrollTop);
    expect(offset(cardAfterFold, viewport)).toBe(afterKeyboard);
  });
  it("keeps coarse-pointer scroll movement while fold correction is pending", () => {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: (query: string) =>
        ({
          matches: query === "(pointer: coarse)",
          addEventListener: () => {},
          removeEventListener: () => {},
        }) as MediaQueryList,
    });
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 1100, cardPageTop: 1150 };
    const sessionId = "coarse-fold-reader-movement";
    saveScrollState(sessionId, { stuck: true, top: metrics.scrollTop });
    const view = render(
      <Harness metrics={metrics} sessionId={sessionId} folded={false} closed={false} streamed={false} />,
    );
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));

    metrics.scrollHeight = 1250;
    metrics.cardPageTop = 1200;
    view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={false} streamed={false} />);
    act(() => viewport.dispatchEvent(new Event("scroll")));

    metrics.scrollTop = 1060;
    act(() => viewport.dispatchEvent(new Event("scroll")));
    act(() => contentObserver(content).fire());
    expect(metrics.scrollTop).toBeLessThan(metrics.scrollHeight - metrics.clientHeight);

    metrics.scrollHeight = 1400;
    act(() => {
      view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={false} streamed={true} />);
      contentObserver(content).fire();
    });
    expect(metrics.scrollTop).toBeLessThan(metrics.scrollHeight - metrics.clientHeight);
  });

  it("continues following final text after a closed run folds without cards", () => {
    const metrics: Metrics = { clientHeight: 100, scrollHeight: 1200, scrollTop: 1100, cardPageTop: 1150 };
    const sessionId = "closed-fold-bottom-pin";
    saveScrollState(sessionId, { stuck: true, top: metrics.scrollTop });
    const view = render(
      <Harness metrics={metrics} sessionId={sessionId} folded={false} closed={false} streamed={false} />,
    );
    const viewport = view.getByTestId("viewport") as HTMLDivElement;
    const content = view.getByTestId("content");
    act(() => viewport.dispatchEvent(new Event("scroll")));

    metrics.scrollHeight = 1100;
    metrics.scrollTop = metrics.scrollHeight - metrics.clientHeight;
    view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={true} streamed={false} />);
    expect(view.container.querySelectorAll("[data-tool-id]")).toHaveLength(0);

    metrics.scrollHeight = 1250;
    act(() => {
      view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={true} streamed={true} />);
      viewport.dispatchEvent(new Event("scroll"));
      contentObserver(content).fire();
    });
    expect(metrics.scrollTop).toBe(metrics.scrollHeight - metrics.clientHeight);
  });
});
