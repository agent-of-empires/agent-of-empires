import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, stopped, agentMessageChunk } from "./helpers/acpMock";

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

  test("a live (non-trashed) session still renders the composer", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-live",
      initialEvents: [agentMessageChunk("hello")],
    });
    await openStructuredSession(page, mock);

    await expect(page.getByTestId("composer-footer")).toBeVisible({ timeout: 10_000 });
  });
});
