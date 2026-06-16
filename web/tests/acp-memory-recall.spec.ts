// Mocked structured-view render of a synthesize-mode memory recall card
// (#2142). Replays a canned ToolCallStarted frame carrying the structured
// `memory_recall` payload so the full render path runs against the real
// bundle: AcpRuntime smuggles it through the tool-call part args,
// StructuredView rebuilds it via pickMemoryRecall, and MemoryRecallCard
// cleans the <system-reminder> envelope + cat -n prefixes and renders the
// body as DOMPurify-sanitized markdown.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

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

test("malformed memory_recall falls back to a generic read card", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-memory-recall-bad",
    initialEvents: [
      {
        ToolCallStarted: {
          tool_call: {
            id: "mem-bad",
            name: "Recalled synthesized memory",
            kind: "read",
            args_preview: "{}",
            // No `mode`: asMemoryRecall rejects it, so the dispatcher
            // must not render the dedicated card.
            memory_recall: { synthesized_text: "x" } as unknown as { mode: string },
            started_at: new Date().toISOString(),
          },
        },
      },
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  // No dedicated synthesize card; the generic read card shows the title.
  await expect(page.getByText("Recalled synthesized memory")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByTestId("memory-recall-synthesized")).toHaveCount(0);
});
