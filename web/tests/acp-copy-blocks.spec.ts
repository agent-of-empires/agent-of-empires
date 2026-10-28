import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { copiedBy } from "./helpers/clipboard";
import { mockAcpSession, openStructuredSession, stopped, toolCallStarted } from "./helpers/acpMock";

const SHOTS = process.env.MESSAGE_COPY_SHOTS;

async function shot(page: Page, name: string) {
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${name}.png` });
}

async function openTranscript(page: Page) {
  const mock = await mockAcpSession(page, {
    title: "story-copy-blocks",
    initialEvents: [
      { UserPromptSent: { text: "Please **fix** the\nthing" } },
      toolCallStarted({
        id: "edit-1",
        name: "Edit",
        kind: "edit",
        args_preview: JSON.stringify({ file_path: "src/a.rs", old_string: "a\nb\n", new_string: "a\nB\n" }),
      }),
      { ToolCallCompleted: { tool_call_id: "edit-1", is_error: false, content: "" } },
      toolCallStarted({ id: "fail-1", name: "mcp__demo__echo", kind: "other", args_preview: '{"text":"hi"}' }),
      { ToolCallCompleted: { tool_call_id: "fail-1", is_error: true, content: "boom: it broke" } },
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);
  await expect(page.getByText("Please")).toBeVisible();
}

test.describe("copy buttons on blocks", () => {
  test.use({ permissions: ["clipboard-read", "clipboard-write"] });

  test("prompt, diff and tool error each copy their own content", async ({ page }) => {
    await openTranscript(page);
    await page.getByText("Please").hover();
    expect(await copiedBy(page, () => page.getByRole("button", { name: "Copy prompt" }).click())).toBe(
      "Please **fix** the\nthing",
    );
    await shot(page, "blocks-prompt");

    await page.getByRole("button").filter({ hasText: /edit/i }).first().click();
    const diff = await copiedBy(page, () => page.getByRole("button", { name: "Copy diff" }).click());
    expect(diff.split("\n").slice(0, 2)).toEqual(["--- src/a.rs", "+++ src/a.rs"]);
    expect(diff).toContain("\n-b\n+B");
    await shot(page, "blocks-diff");

    // A failed card opens on its own.
    expect(await copiedBy(page, () => page.getByRole("button", { name: "Copy error" }).click())).toBe("boom: it broke");
    await shot(page, "blocks-error");
  });
});
