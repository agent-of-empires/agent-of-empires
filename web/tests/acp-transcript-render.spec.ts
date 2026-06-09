// Mocked ports of the live acp-stories transcript-rendering specs,
// replaying canned ACP frames instead of standing up `aoe serve` plus
// the fake agent. Consolidates:
//   - chat-bubble-overflow (long URL / paste / code line, #1469)
//   - composer-send-enter (Enter sends, streamed response renders)
//   - composer-single-newline-renders (#1472)
//   - composer-streamed-response (multi-chunk assembly)

import type { Locator } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import {
  mockAcpSession,
  openStructuredSession,
  waitForComposerConnected,
  agentMessageChunk,
  stopped,
} from "./helpers/acpMock";

// User story (#1469): long unbreakable tokens in structured view messages
// wrap inside the chat bubble instead of forcing a viewport-level
// horizontal scrollbar.
//
// On a narrow viewport an agent message containing an 80+ char autolinked
// URL, a long absolute file path, and a `─` rule line (the shape
// Playwright's list reporter emits) used to push its bubble past
// `max-w-[80%]`; the chat viewport's `overflow-y-auto` then resolved
// `overflow-x` to `auto` and the whole transcript gained a horizontal
// scrollbar.
//
// Fix lives in two places:
//   - `.acp-markdown :where(p, li, blockquote, a)` gets
//     `overflow-wrap: anywhere` (web/src/index.css) so prose tokens wrap and
//     the bubble reports a small min-content width.
//   - the chat viewport gets `overflow-x-hidden` (StructuredView.tsx) as a
//     belt-and-suspenders clamp.
//
// Fenced code blocks keep their own `overflow-x-auto` and must still scroll
// internally without growing the viewport.
test.describe("chat bubble overflow", () => {
  // Narrow viewport so the unbreakable tokens are wider than the bubble.
  test.use({ viewport: { width: 480, height: 800 } });

  const LONG_URL = "https://github.com/njbrake/agent-of-empires/actions/runs/26342421371/job/77546632641";

  // A prose line carrying an unbreakable absolute path token and a run of
  // `─` rule characters. Kept un-indented on purpose so markdown renders it
  // as a paragraph (the surface the fix targets), not an indented code block.
  const PW_PROSE =
    "Failure at /Users/seluj78/aoe/agent-of-empires-worktrees/fix-flaky-pw-tests/web/tests/terminal-focus-shortcut.spec.ts:79:48 ────────────────────────────────────";

  // A fenced code block whose single line is far wider than the bubble. The
  // code container owns its own horizontal scroll; the viewport must not.
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

    // Wait for the agent message (the autolinked URL) to render.
    const link = page.getByRole("link", { name: LONG_URL });
    await expect(link).toBeVisible({ timeout: 10_000 });

    const viewport = page.getByTestId("acp-viewport");
    await expect(viewport).toBeVisible();

    // Core regression: the wrapped content fits, so the viewport never grows
    // a horizontal scroll area. (Pre-fix, the URL/path do not wrap and
    // scrollWidth exceeds clientWidth.)
    await expect
      .poll(async () => viewport.evaluate((el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth))
      .toBeLessThanOrEqual(0);

    // Belt-and-suspenders clamp is in place.
    await expect(viewport).toHaveCSS("overflow-x", "hidden");

    // Fenced code block keeps its own horizontal-scroll affordance: the
    // scroll container's computed overflow-x is auto/scroll (the wrap rule
    // targets p/li/blockquote/a only, so the code container is untouched).
    const codeScroller: Locator = viewport.locator(".acp-markdown .overflow-x-auto").first();
    await expect(codeScroller).toBeVisible();
    const codeOverflowX = await codeScroller.evaluate((el) => getComputedStyle(el).overflowX);
    expect(["auto", "scroll"]).toContain(codeOverflowX);

    // The wrap rule must NOT leak into code: the long line stays a single
    // unwrapped line, so the code <pre>'s content is wider than its box.
    // (scrollWidth reports the full content width even though the bubble's
    // `pre { overflow: hidden }` clips it.) If overflow-wrap leaked here the
    // line would wrap and scrollWidth would collapse to clientWidth.
    const codePre: Locator = codeScroller.locator("pre").first();
    const codeLineUnwrapped = await codePre.evaluate(
      (el) => (el as HTMLElement).scrollWidth > (el as HTMLElement).clientWidth,
    );
    expect(codeLineUnwrapped).toBe(true);
  });
});

// User story: send a message via Enter on desktop.
//
// Drives the structured view composer textarea, types a prompt, presses
// Enter, and asserts the streamed agent response renders into the chat
// area. The prompt POST is captured by the route mock and answered with
// a single agent_message_chunk frame, mirroring the fake agent's
// default turn.
test("send message via Enter renders agent response", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-send-enter",
    onPrompt: () => [agentMessageChunk("Hello from fake ACP agent."), stopped()],
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await composer.fill("hello agent");
  await composer.press("Enter");

  await expect(page.getByText("Hello from fake ACP agent.")).toBeVisible({
    timeout: 10_000,
  });
  // The composer clear runs after the assistant-ui send path resolves,
  // which can race the chunk render above. Give it a bounded window
  // instead of asserting synchronously.
  await expect(composer).toHaveValue("", { timeout: 5_000 });

  // Enter dispatched the typed text as the prompt POST body.
  expect(mock.promptBodies.map((b) => b.text)).toEqual(["hello agent"]);
});

// User story (#1472): a single newline in the composer is preserved in
// the sent user message.
//
// The composer is a plain <textarea>, so a lone shift+enter shows as a
// visible line break while typing. Before the fix the sent bubble ran
// through remark-gfm only and collapsed single newlines to whitespace.
// This drives the real structured view render path (Markdown.tsx ->
// UserText with breaks enabled): type three lines separated by single
// newlines, send, and assert the rendered user bubble keeps them on
// separate rows (two <br> nodes), not one wrapped paragraph.
test("single newlines in a user message render as line breaks", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "story-single-newline" });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const composer = page.getByRole("textbox", { name: /Send a message/i });
  // fill() sets the textarea value verbatim, including the newlines a
  // shift+enter would have inserted; Enter then sends the whole thing.
  await composer.fill("line a\nline b\nline c");
  await composer.press("Enter");

  // The sent user bubble (rounded-br-sm, right-aligned) must preserve
  // the three lines as separate rows: two hard breaks between them.
  const userBubble = page.locator("div.rounded-br-sm").filter({ hasText: "line a" });
  await expect(userBubble).toBeVisible({ timeout: 10_000 });
  await expect(userBubble.locator("br")).toHaveCount(2);
  await expect(userBubble).toContainText("line b");
  await expect(userBubble).toContainText("line c");
});

// User story: streamed agent response renders progressively in the chat.
//
// The prompt POST is answered with three agent_message_chunk frames in
// sequence within a single turn. The structured view reducer at
// web/src/lib/acpTypes.ts appends each chunk; the rendered DOM must show
// the concatenated message after the turn ends.
test("multi-chunk agent response assembles in the transcript", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-stream",
    onPrompt: () => [agentMessageChunk("Once "), agentMessageChunk("upon "), agentMessageChunk("a time."), stopped()],
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await composer.fill("tell me a story");
  await composer.press("Enter");

  await expect(page.getByText("Once upon a time.")).toBeVisible({
    timeout: 10_000,
  });
});
