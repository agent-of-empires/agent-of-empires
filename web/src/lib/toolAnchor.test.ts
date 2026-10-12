// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";

import { resampleToolAnchors, sampleToolAnchors, toolAnchorDelta } from "./toolAnchor";

const rect = (top: number, bottom: number) => ({ top, bottom }) as DOMRect;

function setup(cards: Record<string, [number, number]>) {
  const viewport = document.createElement("div");
  viewport.getBoundingClientRect = () => rect(100, 600);
  document.body.append(viewport);
  const add = (id: string, [top, bottom]: [number, number]) => {
    const el = document.createElement("div");
    el.dataset.toolId = id;
    el.getBoundingClientRect = () => rect(top, bottom);
    viewport.append(el);
    return el;
  };
  for (const [id, span] of Object.entries(cards)) add(id, span);
  return { viewport, add };
}

afterEach(() => {
  document.body.replaceChildren();
});

describe("tool anchors", () => {
  it("samples every card reaching into the viewport, offset from its top edge", () => {
    const { viewport } = setup({ gone: [0, 90], partial: [80, 160], later: [170, 250], below: [600, 700] });
    expect(sampleToolAnchors(viewport)).toMatchObject([
      { id: "partial", offset: -20 },
      { id: "later", offset: 70 },
    ]);
  });

  it.each([
    ["above", { above: [0, 90] }],
    ["below", { below: [600, 700] }],
  ] as const)("samples nothing when every card is %s the viewport", (_where, cards) => {
    const { viewport } = setup(cards);
    expect(sampleToolAnchors(viewport)).toEqual([]);
  });

  it("measures how far a remounted card moved, so scrolling by it holds the place", () => {
    const { viewport, add } = setup({ a: [150, 200] });
    const anchors = sampleToolAnchors(viewport);
    anchors[0]!.el.remove();
    add("a", [190, 240]);
    expect(toolAnchorDelta(viewport, anchors)).toBe(40);
  });

  it("takes over from a card folded out of sight with a later one that was remounted", () => {
    const { viewport, add } = setup({ header: [120, 160], opened: [170, 400] });
    const anchors = sampleToolAnchors(viewport);
    for (const { el } of anchors) el.remove();
    add("opened", [200, 430]);
    expect(toolAnchorDelta(viewport, anchors)).toBe(30);
  });

  it("leaves a surviving card to native layout and ignores everything vanished", () => {
    const { viewport, add } = setup({ a: [150, 200], b: [210, 300] });
    const anchors = sampleToolAnchors(viewport);
    anchors[0]!.el.getBoundingClientRect = () => rect(300, 350);
    expect(toolAnchorDelta(viewport, anchors)).toBe(0);
    // A surviving first card stops the search even though a later one remounted.
    anchors[1]!.el.remove();
    add("b", [260, 350]);
    expect(toolAnchorDelta(viewport, anchors)).toBe(0);
    anchors[0]!.el.remove();
    expect(toolAnchorDelta(viewport, anchors)).toBe(50);
    for (const { id } of anchors) viewport.querySelector(`[data-tool-id="${id}"]`)?.remove();
    expect(toolAnchorDelta(viewport, anchors)).toBe(0);
  });

  it("resamples while every anchor is attached, and keeps pre-fold anchors once one was folded away", () => {
    const { viewport, add } = setup({ a: [150, 200] });
    const anchors = sampleToolAnchors(viewport);
    expect(resampleToolAnchors(viewport, anchors)).not.toBe(anchors);
    expect(resampleToolAnchors(viewport, [])).toHaveLength(1);

    // A scroll event lands in the frame the fold does, before the correction.
    anchors[0]!.el.remove();
    add("a", [190, 240]);
    const kept = resampleToolAnchors(viewport, anchors);
    expect(kept).toBe(anchors);
    expect(toolAnchorDelta(viewport, kept)).toBe(40);
  });

  it("corrects only the fold's shift when the reader scrolled since the sample", () => {
    const { viewport, add } = setup({ a: [150, 200] });
    const anchors = sampleToolAnchors(viewport);
    // The reader scrolled 20px down, and the fold pushed the card 40px down.
    viewport.scrollTop = 20;
    anchors[0]!.el.remove();
    add("a", [170, 220]);
    expect(toolAnchorDelta(viewport, anchors)).toBe(40);
  });

  it("does not count the browser clamping the scroll to a shrunken end as the reader scrolling", () => {
    const { viewport, add } = setup({ a: [150, 200] });
    viewport.scrollTop = 100;
    const anchors = sampleToolAnchors(viewport);
    // The fold shrank the content to 160px, so the scroll clamps from 100 to 60, and the card moved 30px.
    Object.defineProperty(viewport, "scrollHeight", { value: 160, configurable: true });
    Object.defineProperty(viewport, "clientHeight", { value: 100, configurable: true });
    viewport.scrollTop = 60;
    anchors[0]!.el.remove();
    add("a", [180, 230]);
    expect(toolAnchorDelta(viewport, anchors)).toBe(30);
  });

  it("finds the visible cards among hundreds without measuring each one above them", () => {
    const { viewport, add } = setup({});
    let reads = 0;
    for (let i = 0; i < 500; i++) {
      const el = add(`c${i}`, [i * 40, i * 40 + 30]);
      el.getBoundingClientRect = () => {
        reads++;
        return rect(i * 40, i * 40 + 30);
      };
    }
    const ids = sampleToolAnchors(viewport).map((a) => a.id);
    expect(ids).toEqual(Array.from({ length: 13 }, (_, n) => `c${n + 2}`));
    expect(reads).toBeLessThan(40);
  });
});
