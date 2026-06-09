// CI drift guard for the first-run tutorial, plus resolver coverage.
//
// The guard is the cheap, deterministic half of the "tour cannot silently
// break" contract (issue #1513, user story 3): it couples TOUR_STEPS, the
// TOUR_ANCHORS constants, and the actual `data-tour` attributes in component
// source, so renaming or deleting an anchor on either side turns this red in
// milliseconds. The render-time half (an eligible anchor that fails to paint)
// is covered by the Dashboard render test and the live Playwright smoke.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  TOUR_ANCHORS,
  TOUR_STEPS,
  isStepEligible,
  resolveTourSteps,
  type TourAnchorId,
  type TourStep,
} from "../tourSteps";

const SRC_DIR = join(process.cwd(), "src");
// tourSteps.ts itself legitimately contains the `data-tour="..."` template in
// tourSelector(); tests and stories are not shipped UI. Everything else must go
// through the TOUR_ANCHORS constants.
const EXCLUDED = [join("lib", "tourSteps.ts"), "__tests__", ".test.", ".stories."];

function collectSourceFiles(dir: string, acc: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      collectSourceFiles(full, acc);
    } else if (/\.(ts|tsx)$/.test(entry)) {
      acc.push(full);
    }
  }
  return acc;
}

const COMPONENT_SOURCES = collectSourceFiles(SRC_DIR).filter((f) => !EXCLUDED.some((ex) => f.includes(ex)));

const ANCHOR_KEY_BY_VALUE = new Map<TourAnchorId, string>(
  Object.entries(TOUR_ANCHORS).map(([key, value]) => [value, key]),
);

describe("tour drift guard", () => {
  it("every step anchor is a known TOUR_ANCHORS value", () => {
    const known = new Set<string>(Object.values(TOUR_ANCHORS));
    for (const step of TOUR_STEPS) {
      expect(known.has(step.anchor)).toBe(true);
    }
  });

  it("every TOUR_ANCHORS value is used by at least one step (no orphan anchors)", () => {
    const usedByStep = new Set(TOUR_STEPS.map((s) => s.anchor));
    for (const value of Object.values(TOUR_ANCHORS)) {
      expect(usedByStep.has(value)).toBe(true);
    }
  });

  it("step ids are unique", () => {
    const ids = TOUR_STEPS.map((s) => s.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("every anchor a step points at is attached in component source via TOUR_ANCHORS.<key>", () => {
    const usedInSource = new Set<string>();
    const re = /TOUR_ANCHORS\.(\w+)/g;
    for (const file of COMPONENT_SOURCES) {
      const text = readFileSync(file, "utf8");
      for (const m of text.matchAll(re)) usedInSource.add(m[1]);
    }
    for (const step of TOUR_STEPS) {
      const key = ANCHOR_KEY_BY_VALUE.get(step.anchor);
      expect(key, `anchor ${step.anchor} missing from TOUR_ANCHORS`).toBeDefined();
      expect(
        usedInSource.has(key as string),
        `anchor "${step.anchor}" (step "${step.id}") is never attached in component source`,
      ).toBe(true);
    }
  });

  it("no raw data-tour string literals in component source (must use the typed constant)", () => {
    const offenders: string[] = [];
    for (const file of COMPONENT_SOURCES) {
      const text = readFileSync(file, "utf8");
      if (/data-tour=["']/.test(text)) offenders.push(file);
    }
    expect(offenders, `raw data-tour literals found in: ${offenders.join(", ")}`).toEqual([]);
  });
});

describe("resolveTourSteps", () => {
  const all: TourAnchorId[] = Object.values(TOUR_ANCHORS);
  const present = (anchor: TourAnchorId) => all.includes(anchor);

  it("returns only dashboard-scope steps with present anchors on the dashboard", () => {
    const steps = resolveTourSteps({
      scope: "dashboard",
      readOnly: false,
      isDesktop: true,
      hasAnchor: present,
    });
    const ids = steps.map((s) => s.id);
    expect(ids).toContain("sidebar");
    expect(ids).toContain("new-session");
    expect(ids).toContain("topbar-more");
    // acp-only steps must not leak onto the dashboard
    expect(ids).not.toContain("composer");
    expect(ids).not.toContain("right-panel");
  });

  it("drops the writable-only new-session step in read-only mode", () => {
    const steps = resolveTourSteps({
      scope: "dashboard",
      readOnly: true,
      isDesktop: true,
      hasAnchor: present,
    });
    expect(steps.map((s) => s.id)).not.toContain("new-session");
  });

  it("drops desktop-only steps on coarse pointers", () => {
    const steps = resolveTourSteps({
      scope: "structured-view",
      readOnly: false,
      isDesktop: false,
      hasAnchor: present,
    });
    expect(steps.map((s) => s.id)).not.toContain("right-panel");
  });

  it("drops steps whose anchor is absent from the DOM", () => {
    const steps = resolveTourSteps({
      scope: "dashboard",
      readOnly: false,
      isDesktop: true,
      hasAnchor: () => false,
    });
    expect(steps).toEqual([]);
  });

  it("preserves TOUR_STEPS order", () => {
    const steps = resolveTourSteps({
      scope: "structured-view",
      readOnly: false,
      isDesktop: true,
      hasAnchor: present,
    });
    const orderInSource = TOUR_STEPS.map((s) => s.id);
    const resolvedOrder = steps.map((s) => s.id);
    const expected = orderInSource.filter((id) => resolvedOrder.includes(id));
    expect(resolvedOrder).toEqual(expected);
  });

  it("isStepEligible ignores DOM presence (metadata only)", () => {
    const composer = TOUR_STEPS.find((s) => s.id === "composer") as TourStep;
    expect(
      isStepEligible(composer, {
        scope: "structured-view",
        readOnly: false,
        isDesktop: true,
      }),
    ).toBe(true);
    expect(
      isStepEligible(composer, {
        scope: "dashboard",
        readOnly: false,
        isDesktop: true,
      }),
    ).toBe(false);
  });
});
