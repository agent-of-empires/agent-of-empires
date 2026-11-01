// A new Structured view Session that is still starting: one line names the Agent and counts up.

import { test, expect } from "./helpers/mockedTest";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

async function openStarting(page: import("@playwright/test").Page) {
  const mock = await mockAcpSession(page, { title: "story-starting", workerStarting: true });
  await openStructuredSession(page, mock);
  const line = page.getByTestId("startup-wait-line");
  await expect(line).toContainText("Starting Claude Code… You can type now.");
  return line;
}

test("one startup line names the Agent and counts", async ({ page }) => {
  const line = await openStarting(page);
  await expect(page.getByTestId("startup-wait-line")).toHaveCount(1);
  await expect(page.getByText(/structured view worker|few seconds/)).toHaveCount(0);
  await expect(line.getByTestId("startup-wait-elapsed")).toHaveText(/^0:0[0-9]$/);
});

test("a stopped Session offers an explicit Start without granting native send permission", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-absent",
    workerState: "absent",
    acpSessionId: "retained-session",
    sessionFields: { status: "Stopped" },
    nativeAvailable: false,
    promptWakeEligible: false,
  });
  const starts: string[] = [];
  await page.route("**/api/sessions/*/start", (r) => {
    starts.push(r.request().method());
    return r.fulfill({ json: { message: "Session started" } });
  });
  const at = new Date().toISOString();
  await page.route("**/api/sessions/*/queue", (r) =>
    r.fulfill({
      json: [
        { id: "q1", seq: 1, text: "first", created_at: at },
        { id: "q2", seq: 2, text: "second", created_at: at },
      ],
    }),
  );
  await openStructuredSession(page, mock);
  await expect(page.getByRole("button", { name: "first", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "second", exact: true })).toBeVisible();
  const start = page.getByTestId("queued-send-now").first();
  await expect(start).toHaveText("Start and send");
  await expect(start).toBeEnabled();
  await expect(page.getByTestId("startup-wait-line")).toHaveCount(0);
  expect(starts).toEqual([]);
  expect(mock.promptBodies).toEqual([]);
  await start.click();
  await expect.poll(() => starts).toEqual(["POST"]);
  expect(mock.promptBodies).toEqual([]);
});

test("socket-open with unknown native authority cannot send queued intent before a governor receipt", async ({
  page,
}) => {
  const mock = await mockAcpSession(page, {
    title: "story-unknown-authority",
    workerState: "running",
    acpSessionId: "retained-session",
    nativeAvailable: false,
    promptWakeEligible: false,
  });
  const at = new Date().toISOString();
  await page.route("**/api/sessions/*/queue", (r) =>
    r.fulfill({
      json: [
        { id: "q1", seq: 1, text: "first", created_at: at },
        { id: "q2", seq: 2, text: "second", created_at: at },
      ],
    }),
  );
  await openStructuredSession(page, mock);
  const queuedSend = page.getByTestId("queued-send-now");
  await expect(queuedSend).toHaveCount(2);
  await expect(queuedSend.nth(0)).toBeDisabled();
  await expect(queuedSend.nth(1)).toBeDisabled();
  expect(mock.promptBodies).toEqual([]);
  mock.setNativeState({ ready: false, wakeEligible: true });
  await expect(queuedSend.nth(0)).toBeEnabled();
  await expect(queuedSend.nth(1)).toBeEnabled();
  expect(mock.promptBodies).toEqual([]);
  await queuedSend.nth(0).click();
  await expect.poll(() => mock.promptBodies.length).toBe(1);
  expect(mock.promptBodies[0]?.text).toBe("first");
});

test("an older Session resuming never shows the startup line, even before its events load", async ({ page }) => {
  // Counts every DOM change that leaves the line in the page, so a one-frame flash shows up.
  await page.addInitScript(() => {
    const w = window as unknown as { __startupLineSeen: number };
    w.__startupLineSeen = 0;
    new MutationObserver(() => {
      if (document.querySelector('[data-testid="startup-wait-line"]')) w.__startupLineSeen++;
    }).observe(document, { subtree: true, childList: true });
  });
  const mock = await mockAcpSession(page, {
    title: "story-resuming-old",
    workerState: "resuming",
    acpSessionId: "acp-session-1",
    initialEvents: [{ UserPromptSent: { text: "hi", prompt_id: "p1" } }, agentMessageChunk("hello"), stopped()],
  });
  await openStructuredSession(page, mock);
  await expect(page.getByText("hello")).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { __startupLineSeen: number }).__startupLineSeen)).toBe(0);
});

test("an imported Session shows the startup line while its Agent loads the history", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-import",
    workerStarting: true,
    acpSessionId: "imported-acp-session",
    importPending: true,
  });
  await openStructuredSession(page, mock);
  await expect(page.getByTestId("startup-wait-line")).toContainText("Starting Claude Code… You can type now.");
});

test("a running Worker with no stored Agent session shows no startup line while history loads", async ({ page }) => {
  // A forwarded /new leaves the Worker running with no stored Agent session id.
  const mock = await mockAcpSession(page, {
    title: "story-running-cleared",
    workerState: "running",
    initialEvents: [{ UserPromptSent: { text: "hi", prompt_id: "p1" } }, agentMessageChunk("hello")],
  });
  let release!: () => void;
  const gate = new Promise<void>((resolve) => (release = resolve));
  let requested!: () => void;
  const replayRequested = new Promise<void>((resolve) => (requested = resolve));
  await page.route(/\/acp\/replay(\?|$)/, async (r) => {
    requested();
    await gate;
    await r.fallback();
  });
  await openStructuredSession(page, mock);
  await replayRequested;
  await expect(page.getByTestId("startup-wait-line")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Send while disconnected" })).toBeVisible();
  release();
  await expect(page.getByText("hello")).toBeVisible();
  await expect(page.getByTestId("startup-wait-line")).toHaveCount(0);
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the line fits without horizontal scroll and keeps the count visible", async ({ page }) => {
    const line = await openStarting(page);
    await expect(line.getByTestId("startup-wait-elapsed")).toBeInViewport();
    // Nothing overlays the count, such as the header collapse toggle.
    const box = (await line.getByTestId("startup-wait-elapsed").boundingBox())!;
    for (const x of [box.x + box.width / 2, box.x + box.width - 1]) {
      const on = await page.evaluate(
        ([x, y]) => document.elementFromPoint(x!, y!)?.closest("[data-testid]")?.getAttribute("data-testid"),
        [x, box.y + 2],
      );
      expect(on).toBe("startup-wait-elapsed");
    }
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });

  test("a long unspaced Agent name wraps in the line and the queued reason", async ({ page }) => {
    // Underscores, unlike hyphens, give the browser no place to break.
    const agent = "acme_internal_structured_agent_with_a_very_long_registry_name";
    const mock = await mockAcpSession(page, { title: "story-long-agent", workerStarting: true, startingAgent: agent });
    // Two prompts the Daemon already holds for the starting Session.
    const at = new Date().toISOString();
    await page.route("**/api/sessions/*/queue", (r) =>
      r.fulfill({
        json: [
          { id: "q1", seq: 1, text: "first", created_at: at },
          { id: "q2", seq: 2, text: "second", created_at: at },
        ],
      }),
    );
    await openStructuredSession(page, mock);
    const line = page.getByTestId("startup-wait-line");
    await expect(line).toContainText(`Starting ${agent}…`);
    await expect(line.getByTestId("startup-wait-elapsed")).toBeInViewport({ ratio: 1 });
    expect(await line.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);

    await expect(page.getByTestId("queued-strip-header")).toContainText("Queued (2)");
    const reason = page.getByTestId("queued-strip-reason");
    await expect(reason).toHaveText(`· sends when ${agent} is ready`);
    await expect(reason).toBeInViewport({ ratio: 1 });
    const clearAll = page.getByRole("button", { name: "Clear all" });
    await expect(clearAll).toBeInViewport({ ratio: 1 });
    // The reason may wrap Clear all onto its own line, but never under it.
    const [r, c] = [(await reason.boundingBox())!, (await clearAll.boundingBox())!];
    const overlaps = r.x < c.x + c.width && c.x < r.x + r.width && r.y < c.y + c.height && c.y < r.y + r.height;
    expect(overlaps).toBe(false);
    const strip = page.getByTestId("queued-strip");
    expect(await strip.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);
  });
});
