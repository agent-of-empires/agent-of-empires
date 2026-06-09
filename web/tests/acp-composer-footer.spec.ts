// Mocked port of the live acp-stories composer-mobile-footer-actions
// spec, replaying a canned config-options frame instead of standing up
// `aoe serve` plus the fake agent.
//
// User story (#1717): on a narrow mobile viewport the composer footer
// keeps the right-side action button reachable even when the left
// control cluster (mode + model + effort + attachment) is wide.
//
// Pre-fix the footer was a single non-wrapping flex row with
// `justify-between` and no shrink budget, so the populated left cluster
// pushed the Send / Stop cluster past the clipped viewport edge and the
// action button became untappable. The fix lets the left cluster wrap
// onto extra rows and pins the right cluster with `shrink-0`.
//
// A ConfigOptionsUpdated frame advertising model + reasoning-effort
// options (the same snapshot the fake agent emits by default) recreates
// the worst-case left-cluster width. The fix is pure responsive CSS
// (no pointer-capability branch), so a narrow viewport alone reproduces
// it; no coarse-pointer emulation is needed.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, configOptionsUpdated } from "./helpers/acpMock";

// Narrow viewport: the populated left cluster is wider than the row.
test.use({ viewport: { width: 360, height: 740 } });

test("mobile composer footer keeps the Send action reachable when config controls are present", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-footer-actions",
    initialEvents: [
      configOptionsUpdated([
        {
          id: "model",
          name: "Model",
          category: "model",
          current_value: "claude-opus-4-7",
          options: [
            { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
            { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
          ],
        },
        {
          id: "effort",
          name: "Reasoning Effort",
          category: "thought_level",
          current_value: "default",
          options: [
            { value: "default", name: "Default" },
            { value: "low", name: "Low" },
            { value: "medium", name: "Medium" },
            { value: "high", name: "High" },
          ],
        },
      ]),
    ],
  });
  await openStructuredSession(page, mock);

  // The model chip rendering confirms the left cluster carries the
  // config controls that create the width pressure this story guards.
  await expect(page.getByTestId("config-option-model")).toBeVisible({
    timeout: 15_000,
  });

  // Core regression: the footer must not overflow horizontally, so the
  // right action cluster is never pushed past the clipped viewport edge.
  const footer = page.getByTestId("composer-footer");
  await expect(footer).toBeVisible();
  await expect
    .poll(async () => footer.evaluate((el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth))
    .toBeLessThanOrEqual(0);

  // The Send button sits entirely within the viewport (pre-fix its
  // right edge exceeded the 360px viewport width).
  const send = page.getByRole("button", { name: "Send message" });
  await expect(send).toBeVisible();
  const box = await send.boundingBox();
  expect(box).not.toBeNull();
  const viewport = page.viewportSize();
  expect(viewport).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(viewport!.width);

  // And it is actually tappable without a forced click: the click must
  // land and dispatch the prompt POST.
  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await composer.fill("reachable on mobile");
  await send.click();
  await expect.poll(() => mock.promptBodies.length).toBe(1);
  expect(mock.promptBodies[0]!.text).toBe("reachable on mobile");
});
