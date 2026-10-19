import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, stopped, toolCallStarted } from "./helpers/acpMock";

const SHOTS = process.env.TOOL_MEDIA_SHOTS;

const SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="320" height="120"><rect width="320" height="120" fill="#1e3a5f"/><text x="16" y="68" fill="#fff" font-size="22" font-family="sans-serif">screenshot.png</text></svg>`;

test("a tool result's image, link and embedded resource render under its card", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-tool-media",
    initialEvents: [
      toolCallStarted({
        id: "m1",
        name: "mcp__browser__screenshot",
        kind: "other",
        args_preview: '{"url":"https://example.com"}',
      }),
      {
        ToolCallCompleted: {
          tool_call_id: "m1",
          is_error: false,
          content: "Captured example.com",
          output: [
            { kind: "text", text: "Captured example.com" },
            { kind: "image", mime_type: "image/svg+xml", data: Buffer.from(SVG).toString("base64") },
            { kind: "resource_link", uri: "https://example.com/report", name: "Full report" },
            {
              kind: "resource",
              uri: "file:///notes.md",
              mime_type: "text/markdown",
              text: "# Notes\n\n- first finding\n- second finding",
            },
          ],
        },
      },
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  await expect(page.getByAltText("tool output image (image/svg+xml)")).toBeVisible();
  await expect(page.getByRole("link", { name: "Full report" })).toHaveAttribute("href", "https://example.com/report");
  await expect(page.getByText("- first finding")).toBeVisible();
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/tool-output-media.png` });
});
