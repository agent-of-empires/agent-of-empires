import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, backgroundAgentLaunched, stopped } from "./helpers/acpMock";

// Regression for the busy-signal fix (#3900): the main turn ending while a
// background sub-agent (Claude's async Task tool) is still running must not
// leave the composer unable to accept a new prompt.
//
// `AcpRuntime.tsx` briefly fed a combined "is anything busy" signal into
// assistant-ui's `isRunning`, which the library's own `ComposerPrimitive.Input`
// hard-disables Enter under whenever `isRunning && !capabilities.queue` (this
// adapter never declares a queue capability). `Composer.tsx`'s own
// `turnActive`-driven enqueue bypass (the deliberate workaround for that same
// assistant-ui behavior, see #1031) only engages when the real ACP
// `turnActive` is true, so with the main turn idle and only a background
// agent running, `isRunning` had no case sending Enter through: the keystroke
// vanished with no POST, no queue entry, nothing.
test("Enter still submits a prompt while a background agent runs and the main turn is idle", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-bg-agent-composer",
    initialEvents: [
      backgroundAgentLaunched({
        agent_id: "bg-1",
        tool_call_id: "tc-bg-1",
        description: "summarize the backend",
        prompt: "summarize src/",
        model: "claude-opus-4-8",
      }),
      // The main turn ends while the background agent above is still
      // outstanding: turnActive goes false, hasActiveBackgroundAgent stays
      // true. This is exactly the state the swallowed-Enter regression needs.
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await expect(composer).toBeVisible({ timeout: 10_000 });
  await composer.fill("what's next");
  await composer.press("Enter");

  // The core assertion: the prompt actually posted. A swallowed Enter
  // leaves promptBodies empty forever; poll rather than a fixed wait so a
  // slow but eventual send does not falsely fail.
  await expect.poll(() => mock.promptBodies.length, { timeout: 5_000 }).toBe(1);
  expect(mock.promptBodies[0]?.text).toBe("what's next");

  // The textarea clears on a successful send, mirroring every other
  // composer-submit spec's success signal. Re-query by role only: sending
  // opens the turn, so the accessible name has already flipped away from
  // "Send a message".
  await expect(page.getByRole("textbox")).toHaveValue("");
});
