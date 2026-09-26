// Structured view against replayed ACP frames: transcript rendering, tool
// cards, the composer footer, memory-recall cards, transcript font size, the
// trashed read-only state, and the seen-ping telemetry.

import type { Locator, Page } from "@playwright/test";
import { test, expect, waitForResponseBody, publishedRequests, observeFor } from "./helpers/mockedTest";
import {
  agentMessageChunk,
  configOptionsUpdated,
  mockAcpSession,
  openStructuredSession,
  stopped,
  toolCallStarted,
  usageUpdated,
  waitForComposerConnected,
} from "./helpers/acpMock";
import { iPhone13 } from "./helpers/viewports";

const composerBox = (page: Page) => page.getByRole("textbox", { name: /Send a message/i });
const acpViewport = (page: Page) => page.getByTestId("acp-viewport");

/** The element's horizontal overflow, in px. */
const overflowX = (locator: Locator) =>
  locator.evaluate((el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth);

// ─────────────────────────── transcript ───────────────────────────
// #1469: unbreakable tokens wrap inside the bubble instead of scrolling the viewport; fenced code still scrolls itself.
test.describe("chat bubble overflow", () => {
  // Narrow viewport so the unbreakable tokens are wider than the bubble.
  test.use({ viewport: { width: 480, height: 800 } });

  const LONG_URL = "https://github.com/njbrake/agent-of-empires/actions/runs/26342421371/job/77546632641";

  // Un-indented so markdown renders a paragraph, not a code block.
  const PW_PROSE =
    "Failure at /Users/seluj78/aoe/agent-of-empires-worktrees/fix-flaky-pw-tests/web/tests/terminal-focus-shortcut.spec.ts:79:48 ────────────────────────────────────";

  const LONG_CODE_LINE = "const x = " + "a".repeat(200) + ";";

  test("long URL, PW paste, and code line stay inside the chat viewport", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-overflow",
      initialEvents: [
        agentMessageChunk(
          `Run link: ${LONG_URL}\n\n` + `${PW_PROSE}\n\n` + "```ts\n" + `${LONG_CODE_LINE}\n` + "```\n",
        ),
        stopped(),
      ],
    });
    await openStructuredSession(page, mock);

    const link = page.getByRole("link", { name: LONG_URL });
    await expect(link).toBeVisible({ timeout: 10_000 });

    const viewport = acpViewport(page);
    await expect(viewport).toBeVisible();
    await expect.poll(() => overflowX(viewport)).toBeLessThanOrEqual(0);
    await expect(viewport).toHaveCSS("overflow-x", "hidden");

    const codeScroller = viewport.locator(".acp-markdown .overflow-x-auto").first();
    await expect(codeScroller).toBeVisible();
    await expect
      .poll(async () => codeScroller.evaluate((el) => getComputedStyle(el).overflowX))
      .toMatch(/^(auto|scroll)$/);

    // The wrap rule does not reach code, so the line stays wider than its box.
    const codePre = codeScroller.locator("pre").first();
    await expect.poll(() => overflowX(codePre)).toBeGreaterThan(0);

    // #2443: the <pre> scrolls rather than clipping.
    await expect.poll(async () => codePre.evaluate((el) => getComputedStyle(el).overflowX)).toMatch(/^(auto|scroll)$/);
  });
});

test("Enter sends a multi-line message that keeps its line breaks and renders the streamed response as one message", async ({
  page,
}) => {
  const mock = await mockAcpSession(page, {
    title: "story-send-enter",
    onPrompt: () => [
      agentMessageChunk("Hello from "),
      agentMessageChunk("fake ACP "),
      agentMessageChunk("agent."),
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const composer = composerBox(page);
  await composer.fill("hello agent\nline b\nline c");
  await composer.press("Enter");

  await expect(page.getByText("Hello from fake ACP agent.")).toBeVisible({
    timeout: 10_000,
  });
  // The clear can land after the streamed chunk renders.
  await expect(composer).toHaveValue("", { timeout: 5_000 });

  expect(mock.promptBodies.map((b) => b.text)).toEqual(["hello agent\nline b\nline c"]);
  // #1472: single newlines survive in the sent user bubble.
  const userBubble = page.locator("div.rounded-br-sm").filter({ hasText: "hello agent" });
  await expect(userBubble).toBeVisible();
  await expect(userBubble.locator("br")).toHaveCount(2);
  await expect(userBubble).toContainText("line b");
  await expect(userBubble).toContainText("line c");
});

// ─────────────────────────── tool cards ───────────────────────────
// #1568: an edit card's diff scrolls horizontally inside the card; the transcript never does.
test.describe("edit card diff scroll", () => {
  test.use({ viewport: { width: 480, height: 800 } });

  const LONG_LINE = `const x = "${"a".repeat(300)}";`;

  test("edit card diff scrolls horizontally on a narrow viewport", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-edit-scroll",
      initialEvents: [
        toolCallStarted({
          id: "tc-edit-1",
          name: "Edit",
          kind: "edit",
          args_preview: JSON.stringify({
            file_path: "big.txt",
            old_string: "const x = 1;",
            new_string: LONG_LINE,
          }),
        }),
      ],
    });
    await openStructuredSession(page, mock);

    const cardHeader = page.getByRole("button").filter({ hasText: "big.txt" }).first();
    await expect(cardHeader).toBeVisible({ timeout: 10_000 });
    await cardHeader.click();

    const diff = page.getByTestId("string-diff");
    await expect(diff).toBeVisible({ timeout: 10_000 });

    expect(["auto", "scroll"]).toContain(await diff.evaluate((el) => getComputedStyle(el).overflowX));

    // The content really overflows, so the scroll context is not vacuous.
    await expect.poll(() => overflowX(diff)).toBeGreaterThan(0);

    const viewport = acpViewport(page);
    await expect(viewport).toBeVisible();
    await expect.poll(() => overflowX(viewport)).toBeLessThanOrEqual(0);
  });
});

// ─────────────────────────── composer ────────────────────────────
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
      // Usage shows at every width, so its hint competes for footer space (#3916).
      usageUpdated({ used: 1_950_000, size: 2_000_000, cost: { amount: 1234.5678, currency: "EUR" } }),
    ],
  });
  await openStructuredSession(page, mock);

  // The model chip rendering confirms the left cluster carries the
  // config controls that create the width pressure this story guards.
  await expect(page.getByTestId("config-option-model")).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.getByTestId("composer-usage")).toBeVisible();

  // Core regression: the footer must not overflow horizontally, so the
  // right action cluster is never pushed past the clipped viewport edge.
  const footer = page.getByTestId("composer-footer");
  await expect(footer).toBeVisible();
  await expect.poll(() => overflowX(footer)).toBeLessThanOrEqual(0);

  // The Send button sits entirely within the viewport (pre-fix its
  // right edge exceeded the 360px viewport width).
  const send = page.getByRole("button", { name: "Send message" });
  await expect(send).toBeVisible();
  const box = (await send.boundingBox())!;
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(page.viewportSize()!.width);

  // And it is actually tappable without a forced click: the click must
  // land and dispatch the prompt POST.
  const composer = composerBox(page);
  await composer.fill("reachable on mobile");
  await send.click();
  await expect.poll(() => mock.promptBodies.length).toBe(1);
  expect(mock.promptBodies[0]!.text).toBe("reachable on mobile");

  // Mid-turn the cluster holds Stop and Queue, its widest state. An ancestor
  // clips the composer, so the footer's own scroll width cannot see overflow.
  for (const name of ["Stop", "Queue follow-up message"]) {
    const button = page.getByRole("button", { name, exact: true });
    await expect(button).toBeVisible();
    const b = (await button.boundingBox())!;
    expect(b.x + b.width, name).toBeLessThanOrEqual(page.viewportSize()!.width);
  }
});

test("mobile composer shows a compact usage hint inside the viewport", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-usage-mobile",
    initialEvents: [usageUpdated({ used: 120_000, size: 200_000, cost: { amount: 0.42, currency: "USD" } })],
  });
  await openStructuredSession(page, mock);

  const usage = page.getByTestId("composer-usage");
  await expect(usage).toBeVisible({ timeout: 15_000 });
  await expect(usage).toHaveAccessibleName(/Context window: .* tokens used \(60%\)/);
  await expect(usage).toContainText("60%");
  await expect(usage).toContainText("0.42");
  // Compact variant: token counts are desktop-only.
  await expect(usage.getByText("120k/200k")).toBeHidden();

  const box = (await usage.boundingBox())!;
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(page.viewportSize()!.width);
});

// ────────────────────────── memory recall ─────────────────────────
const DIRTY =
  "<system-reminder>\n     1\t# User profile\n     2\t\n     3\tUser is a senior engineer.\n     4\t\n     5\t- terse\n     6\t- no em dashes\n</system-reminder>";

test("synthesize memory recall renders cleaned, sanitized markdown", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-memory-recall",
    initialEvents: [
      {
        ToolCallStarted: {
          tool_call: {
            id: "mem-1",
            name: "Recalled synthesized memory",
            kind: "read",
            args_preview: "{}",
            started_at: new Date().toISOString(),
            memory_recall: { mode: "synthesize", synthesized_text: DIRTY },
          },
        },
      },
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  // Card lands collapsed; its header carries the synthesize label.
  const header = page.getByRole("button").filter({ hasText: "Synthesised memory" }).first();
  await expect(header).toBeVisible({ timeout: 10_000 });
  await header.click();

  const body = page.getByTestId("memory-recall-synthesized");
  await expect(body).toBeVisible();
  await expect(body).toContainText("User is a senior engineer.");
  // Transport noise stripped, markdown rendered to elements.
  await expect(body).not.toContainText("system-reminder");
  await expect(body.locator("h1")).toHaveText("User profile");
  await expect(body.locator("li")).toHaveCount(2);
});

// ─────────────────────────── font size ───────────────────────────

// The transcript font size has mobile and desktop values chosen by
// clientFormFactor() (coarse pointer and under 768px). A browser test because
// jsdom evaluates neither pointer media nor rem; pointer capability is fixed
// per context, so each describe owns one and resizes live.
const MOBILE_SIZE = 11;
const DESKTOP_SIZE = 20;

async function openTranscript(page: Page) {
  await page.addInitScript(
    ([mobile, desktop]) => {
      window.localStorage.setItem(
        "aoe-web-settings",
        JSON.stringify({ structuredMobileFontSize: mobile, structuredDesktopFontSize: desktop }),
      );
    },
    [MOBILE_SIZE, DESKTOP_SIZE],
  );

  const mock = await mockAcpSession(page, {
    title: "story-font-size",
    initialEvents: [agentMessageChunk("# heading\n\nplain paragraph text\n\n```\nfenced code\n```"), stopped()],
  });
  await openStructuredSession(page, mock);

  const body = page.locator(".acp-markdown-body").first();
  await expect(body).toBeVisible({ timeout: 10_000 });
  return body;
}

const fontSizeOf = (locator: Locator) => locator.evaluate((el) => getComputedStyle(el).fontSize);

const leadingRatioOf = (locator: Locator) =>
  locator.evaluate((el) => {
    const cs = getComputedStyle(el);
    return Number.parseFloat(cs.lineHeight) / Number.parseFloat(cs.fontSize);
  });

test.describe("structured view conversation font size (fine pointer)", () => {
  test.use({ viewport: { width: 1200, height: 800 }, hasTouch: false });

  test("uses the desktop size at any width and scales it with the browser root font size", async ({ page }) => {
    const body = await openTranscript(page);
    const heading = body.locator("h1").first();

    expect(await fontSizeOf(body)).toBe("20px");
    expect(await fontSizeOf(heading)).toBe("28.6px");

    // Code keeps its tight leading: --tw-leading does not inherit, so without its own it would take leading-relaxed.
    const codeBlock = body.locator("pre").first();
    expect(await fontSizeOf(codeBlock)).toBe("17.2px");
    expect(await leadingRatioOf(codeBlock)).toBeCloseTo(1.3333, 3);

    await page.setViewportSize({ width: 500, height: 800 });
    await expect.poll(() => fontSizeOf(body)).toBe("20px");

    // Published in rem, so a larger root scales the transcript.
    await page.evaluate(() => {
      document.documentElement.style.fontSize = "20px";
    });
    await expect.poll(() => fontSizeOf(body)).toBe("25px");
  });
});

test.describe("structured view conversation font size (coarse pointer)", () => {
  test.use(iPhone13);

  test("uses the mobile size when narrow and the desktop size once the viewport widens", async ({ page }) => {
    const body = await openTranscript(page);
    const heading = body.locator("h1").first();

    expect(await fontSizeOf(body)).toBe("11px");
    expect(await fontSizeOf(heading)).toBe("15.73px");

    await page.setViewportSize({ width: 900, height: 800 });
    await expect.poll(() => fontSizeOf(body)).toBe("20px");
    expect(await fontSizeOf(heading)).toBe("28.6px");
  });
});

// ──────────────────────────── trashed ─────────────────────────────
// User story (#2529): a trashed structured-view session is read-only until
// restored. The transcript stays visible under the trashed banner, but the
// queue strips and composer are gone: the reconciler will never resume a
// trashed session, so any input would only stash into a queue that never
// drains.

test.describe("trashed structured session is read-only", () => {
  test("renders the trashed banner and no composer", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-trashed",
      trashedAt: new Date().toISOString(),
      // A transcript line plus a user_stopped worker so the trashed banner
      // (which gates on workerStopped) shows, matching a real trashed session.
      initialEvents: [agentMessageChunk("earlier reply"), stopped("user_stopped")],
    });
    await openStructuredSession(page, mock);

    await expect(page.getByTestId(`acp-trashed-banner-${mock.sessionId}`)).toBeVisible({ timeout: 10_000 });
    // The transcript is still shown read-only.
    await expect(page.getByText("earlier reply")).toBeVisible();
    // No composer / send affordance for a session that cannot be resumed.
    await expect(page.getByTestId("composer-footer")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Send message" })).toHaveCount(0);
  });
});

// ────────────────────────── seen telemetry ────────────────────────
test("opening a structured view session fires the structured view seen-ping", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "acp-seen-ping" });
  await openStructuredSession(page, mock);

  // The structured view mount fires `reportTelemetrySeen("structured_view")`.
  // Pre-fix no caller passed `"structured_view"`, so this poll timed out
  // (the bug).
  await expect
    .poll(() => mock.telemetryPings.some((p) => p.surface === "structured_view"), {
      timeout: 10_000,
    })
    .toBe(true);

  // The on-load `"web"` ping still fires too; the structured view ping is
  // additive, not a replacement.
  expect(mock.telemetryPings.some((p) => p.surface === "web")).toBe(true);
});

test("a read-only server sends no telemetry seen-ping", async ({ page }) => {
  // The seen-ping effects (both `"web"` and `"structured_view"`) share the
  // same guard: skip on read-only servers, which can't persist a snapshot.
  const mock = await mockAcpSession(page, { about: { read_only: true } });

  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  await waitForResponseBody(page, "/api/about");
  await expect(page.getByTestId("sidebar-session-row")).toHaveCount(1);
  await observeFor(page, 500, async () => {
    expect(await publishedRequests(page, "/api/telemetry/seen", "POST")).toEqual([]);
    expect(mock.telemetryPings).toEqual([]);
  });
});
