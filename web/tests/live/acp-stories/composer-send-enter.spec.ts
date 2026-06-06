// User story: send a message via Enter on desktop.
//
// Drives the structured view composer textarea, types a prompt, presses Enter,
// and asserts the streamed agent response renders into the chat area.
// The default fake-ACP turn emits a single agent_message_chunk with
// "Hello from fake ACP agent." then stops.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import {
  waitForStructuredView,
  enableStructuredViewAndWait,
} from "../../helpers/acp";

base(
  "send message via Enter renders agent response",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-send-enter" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-send-enter");
      if (!seeded) throw new Error("seeded session 'story-send-enter' missing");
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.fill("hello agent");
      await composer.press("Enter");

      await expect(page.getByText("Hello from fake ACP agent.")).toBeVisible({
        timeout: 10_000,
      });
      // The composer clear runs after the assistant-ui send path
      // resolves, which can race the chunk render above. Give it a
      // bounded window instead of asserting synchronously.
      await expect(composer).toHaveValue("", { timeout: 5_000 });
    } finally {
      await serve.stop();
    }
  },
);
