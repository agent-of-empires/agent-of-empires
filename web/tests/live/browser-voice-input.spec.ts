// Real browser voice bridge acceptance test. Chromium uses its deterministic
// fake microphone, while every AoE boundary remains real: MediaRecorder, REST
// route, capability/action gates, plugin host, worker JSON-RPC, shared UI-state
// polling, and capture-scoped Composer insertion.

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../helpers/liveTest";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd, spawnAoeServe } from "../helpers/aoeServe";

const fixtureDir = resolve(dirname(fileURLToPath(import.meta.url)), "..", "fixtures", "browser-voice-plugin");

test.use({
  launchOptions: {
    args: ["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"],
  },
  permissions: ["microphone"],
});

test("browser microphone crosses the daemon and worker, then applies only in the initiating tab", async ({
  browser,
  page,
}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (seedEnv) => {
      seedSessionViaAoeAdd({ title: "browser-voice-live" })(seedEnv);
      const installed = spawnSync(resolveAoeBinary(), ["plugin", "install", fixtureDir, "--yes"], {
        env: seedEnv.env,
      });
      if (installed.status !== 0) {
        throw new Error(
          `voice fixture install failed: status=${installed.status} error=${installed.error?.message ?? "<none>"} stderr=${installed.stderr?.toString() ?? "<none>"}`,
        );
      }
    },
  });
  try {
    const observerContext = await browser.newContext({ permissions: ["microphone"] });

    try {
      const observer = await observerContext.newPage();
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      const sessionId = sessions[0]!.id;
      await enableStructuredViewAndWait(serve.baseUrl, sessionId, 30_000, serve.home);

      const sessionUrl = `${serve.baseUrl}/session/${sessionId}`;
      await Promise.all([page.goto(sessionUrl), observer.goto(sessionUrl)]);
      await Promise.all([waitForStructuredView(page), waitForStructuredView(observer)]);

      const composer = page.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
      const observerComposer = observer.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
      await composer.fill("before OLD after");
      await composer.evaluate((element: HTMLTextAreaElement) => element.setSelectionRange(7, 10));
      await observerComposer.fill("observer draft stays private");

      const dictate = page.locator('[data-testid="plugin-composer-action"][data-browser-action="voice-input"]');
      await expect(dictate).toBeVisible({ timeout: 15_000 });
      await expect(dictate).toContainText("Dictate");
      const upload = page.waitForResponse(
        (response) => response.url().includes("/browser-voice-input") && response.request().method() === "POST",
      );
      await dictate.click();
      await expect(dictate).toHaveAttribute("data-voice-phase", "recording");
      await page.waitForTimeout(1_250);
      await dictate.click();

      expect((await upload).status()).toBe(202);
      await expect(composer).toHaveValue("before dictated by the live worker after", { timeout: 15_000 });
      await expect(observerComposer).toHaveValue("observer draft stays private");

      const markerPath = join(
        appDirFor(serve.home, join(serve.home, "config"), resolveAoeBinary()),
        "plugins",
        "dev.aoe.browser-voice-e2e",
        "voice-received.json",
      );
      const marker = JSON.parse(readFileSync(markerPath, "utf8"));
      expect(marker).toMatchObject({
        ok: true,
        leaked_composer: false,
      });
      expect(marker.bytes).toBeGreaterThan(0);
      expect(marker.duration_ms).toBeGreaterThan(0);

      const screenshotPath =
        process.env.AOE_VOICE_SCREENSHOT_PATH ?? testInfo.outputPath("browser-voice-round-trip.png");
      await page.screenshot({ path: screenshotPath });
      await testInfo.attach("browser-voice-round-trip", { path: screenshotPath, contentType: "image/png" });
    } finally {
      await observerContext.close();
    }
  } finally {
    await serve.stop();
  }
});
