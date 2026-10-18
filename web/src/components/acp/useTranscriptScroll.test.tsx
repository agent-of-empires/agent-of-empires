// @vitest-environment jsdom

import { act, render } from "@testing-library/react";
import { useCallback, useLayoutEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useTranscriptScroll } from "./useTranscriptScroll";
import { saveScrollState } from "../../lib/acpScrollState";
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

function Harness({ metrics, sessionId, folded, closed, streamed }: HarnessProps) {
  const { viewportRef, belowViewportRef, messagesContentRef } = useTranscriptScroll({
    sessionId,
    canLoadEarlierHistory: false,
    loadEarlierHistory: () => {},
    loadingEarlierHistory: false,
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
  if (originalMatchMedia) Object.defineProperty(window, "matchMedia", originalMatchMedia);
  else Reflect.deleteProperty(window, "matchMedia");
});

describe("useTranscriptScroll fold corrections", () => {
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
    act(() => {
      view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={true} streamed={false} />);
      viewport.dispatchEvent(new Event("scroll"));
      contentObserver(content).fire();
    });
    expect(view.container.querySelectorAll("[data-tool-id]")).toHaveLength(0);

    metrics.scrollHeight = 1250;
    act(() => {
      view.rerender(<Harness metrics={metrics} sessionId={sessionId} folded={true} closed={true} streamed={true} />);
      contentObserver(content).fire();
    });
    expect(metrics.scrollTop).toBe(metrics.scrollHeight - metrics.clientHeight);
  });
});
