import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { backgroundAgentLaunched, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

test.use({ ...devices["iPhone 13"] });

// An async sub-agent opens in the Sub agents view with the same Input, Activity
// and Output layout the inline card uses, and steps between agents on a phone.
test.describe("mobile sub-agent view", () => {
  test("opens an agent full screen and steps to the next one", async ({ page }) => {
    const launch = (n: number) =>
      backgroundAgentLaunched({
        agent_id: `bg-${n}`,
        tool_call_id: `tc-bg-${n}`,
        description: `agent number ${n}`,
        prompt: `prompt for agent ${n}`,
        model: "claude-opus-4-8",
      });
    const mock = await mockAcpSession(page, {
      title: "story-subagent-mobile",
      initialEvents: [launch(1), launch(2), stopped()],
    });
    await openStructuredSession(page, mock);

    await page.getByRole("button", { name: "Toggle panels" }).click();
    await page.getByText("Sub agents", { exact: true }).first().click();
    await page
      .getByRole("button", { name: /agent number/ })
      .first()
      .click();

    await expect(page.getByText("Subagent 1 of 2")).toBeVisible();
    for (const section of ["Input", "Activity", "Output"]) {
      await expect(page.getByText(section, { exact: true }).first()).toBeVisible();
    }
    await page.getByRole("button", { name: "Next subagent" }).click();
    await expect(page.getByText("Subagent 2 of 2")).toBeVisible();
  });
});
