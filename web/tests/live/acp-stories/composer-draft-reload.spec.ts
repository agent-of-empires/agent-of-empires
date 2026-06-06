// User story: composer draft persists across a full page reload.
//
// The Composer mirrors the textarea into localStorage at
// `acp:draft:<sessionId>` with a 250ms debounce. After a reload
// the StructuredView's mount effect seeds the composer from the same key
// so the user does not lose their in-progress prompt.

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

base("composer draft survives a full reload", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-draft-reload" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-draft-reload");
    if (!seeded) throw new Error("seeded session 'story-draft-reload' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
    );
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("unsent draft text");
    // Deterministic poll for the debounced localStorage write instead of
    // a fixed sleep so the assertion is robust on slow CI runners.
    await expect
      .poll(
        async () =>
          await page.evaluate(
            (id) => localStorage.getItem(`acp:draft:${id}`),
            sessionId,
          ),
        { timeout: 5_000 },
      )
      .toBe("unsent draft text");

    await page.reload();
    await waitForStructuredView(page);

    await expect(
      page.getByRole("textbox", { name: /Send a message/i }),
    ).toHaveValue("unsent draft text", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
