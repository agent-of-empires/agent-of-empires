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

  // #4116: an archived session cannot start either, so it gets no composer that would drop a prompt.
  test("renders the archived banner and no composer", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-archived",
      archivedAt: new Date().toISOString(),
      initialEvents: [agentMessageChunk("earlier reply"), stopped("user_stopped")],
    });
    await openStructuredSession(page, mock);

    await expect(page.getByTestId(`acp-archived-banner-${mock.sessionId}`)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("earlier reply")).toBeVisible();
    await expect(page.getByTestId("composer-footer")).toHaveCount(0);
  });

  test("an empty archived session offers no starter prompts", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-archived-empty",
      archivedAt: new Date().toISOString(),
      initialEvents: [stopped("user_stopped")],
    });
    await openStructuredSession(page, mock);

    await expect(page.getByTestId(`acp-archived-banner-${mock.sessionId}`)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("Ask the agent anything about this workspace.")).toHaveCount(0);
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

// ───────────────────── tool run folding keeps the reader's place ─────────────────────
// A run folds into a group once text follows it. A card the reader has open must
// stay open, and neither it nor the scroll offset may move. Browser-only: jsdom has no layout.
test.describe("tool run folding", () => {
  test.use({ viewport: { width: 1200, height: 700 }, hasTouch: false });

  // Scrolls away from the bottom and waits for it to hold. The transcript samples its anchor on the
  // scroll event, a frame after a programmatic scroll, and a late height change can re-pin it first.
  const scrollAwayFromBottom = (page: Page, px: number) =>
    expect
      .poll(async () => {
        const distance = await acpViewport(page).evaluate((el, away) => {
          el.scrollTop = el.scrollHeight - el.clientHeight - away;
          return new Promise<number>((resolve) =>
            requestAnimationFrame(() =>
              requestAnimationFrame(() => resolve(el.scrollHeight - el.clientHeight - el.scrollTop)),
            ),
          );
        }, px);
        return Math.round(distance);
      })
      .toBe(px);

  // An unfinished turn: the agent is busy, so its trailing run may still grow.
  const liveTurn = { UserPromptSent: { text: "go", prompt_id: "p-fold" } };

  const readTool = (n: number) => ({
    id: `fold-${n}`,
    name: "Read",
    kind: "read",
    args_preview: JSON.stringify({ file_path: `/tmp/fold-${n}.rs` }),
  });
  const completed = (n: number) => ({
    ToolCallCompleted: { tool_call_id: `fold-${n}`, content: `OUTPUT-${n}` },
  });

  test("an opened card stays open and in place while its run folds", async ({ page }) => {
    const filler = Array.from({ length: 40 }, (_, i) => `Filler paragraph ${i + 1}.`).join("\n\n");
    const mock = await mockAcpSession(page, {
      title: "story-fold-in-place",
      initialEvents: [
        liveTurn,
        agentMessageChunk(filler),
        ...[1, 2, 3, 4].flatMap((n) => [toolCallStarted(readTool(n)), completed(n)]),
      ],
    });
    await openStructuredSession(page, mock);

    const viewport = acpViewport(page);
    const card = page.locator('[data-tool-id="fold-2"]');
    await expect(card).toBeVisible({ timeout: 10_000 });
    // Still growing, so the run has not folded.
    await expect(page.getByText("4 actions")).toHaveCount(0);

    await card.getByRole("button").first().click();
    await expect(page.getByText("OUTPUT-2")).toBeVisible();
    // Read mid-transcript, away from the bottom pin.
    await scrollAwayFromBottom(page, 120);
    const before = await card.boundingBox();
    const scrollBefore = await viewport.evaluate((el) => el.scrollTop);

    mock.pushEvents([agentMessageChunk("Moving on.")]);
    await expect(page.getByText("4 actions")).toBeVisible();

    await expect(page.getByText("OUTPUT-2")).toBeVisible();
    await expect(page.getByText("OUTPUT-1")).toHaveCount(0);
    const after = await card.boundingBox();
    expect(Math.abs(after!.y - before!.y)).toBeLessThanOrEqual(1);
    const scrollAfter = await viewport.evaluate((el) => el.scrollTop);
    // The group header sits above the card, so the offset moves by the header height to hold it.
    expect(scrollAfter).toBeGreaterThan(scrollBefore);
  });

  // 13 calls fold into chunks of 10 and 3. The first visible card is a header the
  // fold swallows; the reader's open card sits in the other chunk and must hold.
  test("an opened card holds its place when a run spanning two chunks folds", async ({ page }) => {
    const output = Array.from({ length: 30 }, (_, i) => `line ${i + 1}`).join("\n");
    const calls = Array.from({ length: 13 }, (_, i) => i + 1);
    const mock = await mockAcpSession(page, {
      title: "story-fold-two-chunks",
      initialEvents: [
        liveTurn,
        agentMessageChunk(Array.from({ length: 20 }, (_, i) => `Filler paragraph ${i + 1}.`).join("\n\n")),
        ...calls.flatMap((n) => [
          toolCallStarted(readTool(n)),
          { ToolCallCompleted: { tool_call_id: `fold-${n}`, content: n === 11 ? output : `OUTPUT-${n}` } },
        ]),
      ],
    });
    await openStructuredSession(page, mock);

    const viewport = acpViewport(page);
    const opened = page.locator('[data-tool-id="fold-11"]');
    await expect(opened).toBeVisible({ timeout: 10_000 });
    await opened.getByRole("button").first().click();
    await expect(opened).toContainText("line 1");

    // Unpinned (the bottom pin would follow the new text), with a collapsed header of the first chunk first in view.
    await scrollAwayFromBottom(page, 200);
    const firstVisible = await viewport.evaluate((el) => {
      const top = el.getBoundingClientRect().top;
      const card = [...el.querySelectorAll("[data-tool-id]")].find((c) => c.getBoundingClientRect().bottom > top);
      return Number(card?.getAttribute("data-tool-id")?.replace("fold-", ""));
    });
    expect(firstVisible).toBeLessThanOrEqual(10);
    await expect(opened).toBeInViewport();
    const before = await opened.boundingBox();

    // Enough text that the transcript can still scroll after the fold.
    mock.pushEvents([agentMessageChunk(Array.from({ length: 60 }, (_, i) => `After ${i + 1}.`).join("\n\n"))]);
    await expect(page.getByText("10 actions")).toBeVisible();
    await expect(page.getByText("3 actions")).toBeVisible();

    await expect(opened).toContainText("line 1");
    const after = await opened.boundingBox();
    expect(Math.abs(after!.y - before!.y)).toBeLessThanOrEqual(1);
  });

  // The fold lands while following the bottom. Anchors it left behind must not
  // shift the view when the reader later scrolls up and more text streams in.
  test("a fold while pinned leaves no stale anchors behind", async ({ page }) => {
    const filler = Array.from({ length: 40 }, (_, i) => `Filler paragraph ${i + 1}.`).join("\n\n");
    const mock = await mockAcpSession(page, {
      title: "story-fold-pinned",
      initialEvents: [
        liveTurn,
        agentMessageChunk(filler),
        ...[1, 2, 3, 4].flatMap((n) => [toolCallStarted(readTool(n)), completed(n)]),
      ],
    });
    await openStructuredSession(page, mock);

    const card = page.locator('[data-tool-id="fold-2"]');
    await expect(card).toBeVisible({ timeout: 10_000 });
    await card.getByRole("button").first().click();
    await expect(page.getByText("OUTPUT-2")).toBeVisible();

    mock.pushEvents([agentMessageChunk("Moving on.")]);
    const header = page.getByText("4 actions");
    await expect(header).toBeVisible();

    await scrollAwayFromBottom(page, 250);
    const before = (await header.boundingBox())!.y;
    mock.pushEvents([agentMessageChunk(Array.from({ length: 30 }, (_, i) => `After ${i + 1}.`).join("\n\n"))]);
    await expect(page.getByText("After 30.")).toBeAttached();
    expect(Math.abs((await header.boundingBox())!.y - before)).toBeLessThanOrEqual(1);
  });

  // A turn ending on tool calls has no closing text, so the end of the turn folds the run.
  test("a run folds when the turn ends, keeping an opened card in place", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-fold-turn-end",
      initialEvents: [
        liveTurn,
        agentMessageChunk(Array.from({ length: 40 }, (_, i) => `Filler paragraph ${i + 1}.`).join("\n\n")),
        ...[1, 2, 3, 4].flatMap((n) => [toolCallStarted(readTool(n)), completed(n)]),
      ],
    });
    await openStructuredSession(page, mock);

    const card = page.locator('[data-tool-id="fold-2"]');
    await expect(card).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("4 actions")).toHaveCount(0);
    await card.getByRole("button").first().click();
    await expect(page.getByText("OUTPUT-2")).toBeVisible();
    // A reader following the bottom follows it through the fold; this one reads above it.
    await scrollAwayFromBottom(page, 120);
    const before = await card.boundingBox();

    mock.pushEvents([stopped()]);
    await expect(page.getByText("4 actions")).toBeVisible();
    await expect(page.getByText("OUTPUT-2")).toBeVisible();
    expect(Math.abs((await card.boundingBox())!.y - before!.y)).toBeLessThanOrEqual(1);
  });

  test("a run nothing was opened in folds collapsed once text follows", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-fold-collapsed",
      initialEvents: [liveTurn, ...[1, 2, 3].flatMap((n) => [toolCallStarted(readTool(n)), completed(n)])],
    });
    await openStructuredSession(page, mock);

    await expect(page.locator('[data-tool-id="fold-3"]')).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("3 actions")).toHaveCount(0);

    mock.pushEvents([agentMessageChunk("Done.")]);
    await expect(page.getByText("3 actions")).toBeVisible();
    await expect(page.locator('[data-tool-id="fold-1"]')).toHaveCount(0);
  });
});
