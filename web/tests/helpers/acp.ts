import { expect, type Locator, type Page, type TestInfo } from "@playwright/test";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

/** Best-effort read of the per-test debug.log written by `aoe serve`
 *  and its child runners. Returns the last `tailBytes` chars or null
 *  if the file can't be read. Used by enableStructuredViewAndWait to surface
 *  supervisor + runner logs in test failure messages, since the
 *  structured view/enable endpoint returns 200 even when the background spawn
 *  task wedges or fails. */
function readDebugLogTail(home: string | undefined, tailBytes = 8_000): string | null {
  if (!home) return null;
  // Linux: debug.log lives under $XDG_CONFIG_HOME (the harness sets
  // this to `${home}/config`). macOS/Windows: under
  // `${home}/.agent-of-empires-dev`. Probe both so the tail surfaces
  // on whichever OS the test is running.
  const candidates = [
    join(home, "config", "agent-of-empires-dev", "debug.log"),
    join(home, ".agent-of-empires-dev", "debug.log"),
  ];
  const path = candidates.find((p) => existsSync(p));
  if (!path) return null;
  try {
    const content = readFileSync(path, "utf8");
    return content.length > tailBytes
      ? `... (${content.length - tailBytes} bytes elided)\n` + content.slice(-tailBytes)
      : content;
  } catch {
    return null;
  }
}

/**
 * Wait for the structured view supervisor to be ready to accept prompts after a
 * `POST /api/sessions/<id>/acp/enable`.
 *
 * `structured view/enable` is fire-and-forget: it flips the per-session
 * `structured_view` flag and `tokio::spawn`s the supervisor, returning
 * before the ACP `initialize` + `session/new` handshake completes.
 * Sending a `/acp/prompt` immediately races that handshake and the
 * prompt can be lost or queued unpredictably, which surfaces as flaky
 * test failures (notably `acp-approval.spec.ts` and
 * `acp-spawn-prompt.spec.ts` used to do this with a hardcoded
 * `setTimeout(2_000)` that proved tight under CI load).
 *
 * Two-phase wait:
 *   1. Poll `/acp/replay` until any frame appears (proves the
 *      worker process is up and the supervisor finished `initialize`).
 *   2. Poll `/api/sessions` until the seeded session reports
 *      `acp_worker_state === "running"` (proves the ACP
 *      `session/new` handshake completed and the supervisor is ready
 *      to forward prompts).
 *
 * Without phase 2 the React client receives `acp_worker_state ===
 * "resuming"` from the initial `/api/sessions` fetch on navigation,
 * the `useStructuredView` drain effect parks the prompt, and the test races
 * the next REST poll cycle (a few seconds at best, indefinitely under
 * CI load) before "Send a message" Enter actually reaches the
 * supervisor.
 *
 * Default timeout is 15s; the local happy-path completes in well under
 * 1s, so 15s only kicks in on contended CI runners.
 */
async function fetchReplayFrames(baseUrl: string, sessionId: string): Promise<unknown[]> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  if (Array.isArray(replay)) return replay;
  return (replay?.frames as unknown[]) ?? [];
}

/** Look for an `AgentStartupError` event in the replay buffer. The
 *  supervisor publishes this when `acp_enable` succeeds but the
 *  background `supervisor.spawn(...)` task fails (e.g. container ensure
 *  error, agent process spawn refused, handshake timed out). Phase 1 of
 *  waitForAcpReady sees the error frame as "any frame present" and
 *  resolves, but phase 2 (worker_state === "running") never will.
 *  Returning the error string here lets the caller throw with the real
 *  cause instead of timing out on a stale "absent". */
function findStartupError(frames: unknown[]): string | null {
  for (const f of frames) {
    if (typeof f !== "object" || f == null) continue;
    const event = (f as { event?: unknown }).event;
    if (event && typeof event === "object" && "AgentStartupError" in event) {
      const err = (event as { AgentStartupError?: { message?: string } }).AgentStartupError;
      return err?.message ?? "AgentStartupError (no message)";
    }
  }
  return null;
}

export async function waitForAcpReady(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  try {
    await expect
      .poll(
        async () => {
          const frames = await fetchReplayFrames(baseUrl, sessionId);
          const startupErr = findStartupError(frames);
          if (startupErr) {
            throw new Error(`acp_enable spawn failed: ${startupErr} ` + `(frames=${frames.length})`);
          }
          return frames.length;
        },
        { timeout: timeoutMs, intervals: [100, 200, 200, 200] },
      )
      .toBeGreaterThan(0);
  } catch (err) {
    const log = readDebugLogTail(home) ?? "(debug.log unavailable)";
    throw new Error(
      `waitForAcpReady phase 1 (replay frames) failed after ${timeoutMs}ms.\n` +
        `acp/enable returned 200 but no event ever reached the replay buffer, ` +
        `which means the supervisor.spawn task is wedged.\n` +
        `Original: ${err instanceof Error ? err.message : String(err)}\n` +
        `--- debug.log tail ---\n${log}\n--- end debug.log ---`,
      { cause: err },
    );
  }
  // Phase 2: wait for the supervisor to reach "running". On failure
  // dump everything we can about the session + replay frames so the
  // root cause is visible without re-running with debug flags.
  try {
    await expect
      .poll(
        async () => {
          const res = await fetch(`${baseUrl}/api/sessions`);
          if (!res.ok) return "fetch-failed";
          const body = await res.json();
          const sessions: Array<{ id: string; acp_worker_state?: string }> = Array.isArray(body)
            ? body
            : (body.sessions ?? []);
          const me = sessions.find((s) => s.id === sessionId);
          const state = me?.acp_worker_state ?? "absent";
          if (state === "absent") {
            const frames = await fetchReplayFrames(baseUrl, sessionId);
            const startupErr = findStartupError(frames);
            if (startupErr) {
              throw new Error(`acp_enable spawn failed: ${startupErr} ` + `(frames=${frames.length})`);
            }
          }
          return state;
        },
        { timeout: timeoutMs, intervals: [100, 200, 200, 500, 1000] },
      )
      .toBe("running");
  } catch (err) {
    // Augment the timeout error with the full replay + sessions
    // snapshot. expect.poll's default message is just the last polled
    // value, which makes a "stuck absent" indistinguishable from a
    // misrouted poll. Surface what the server actually said.
    const frames = await fetchReplayFrames(baseUrl, sessionId).catch(() => []);
    const sessionsRes = await fetch(`${baseUrl}/api/sessions`).catch(() => null);
    const sessionsBody = sessionsRes ? await sessionsRes.json().catch(() => null) : null;
    const summary = JSON.stringify(
      {
        sessionId,
        sessions: sessionsBody,
        framesCount: frames.length,
        firstFrames: frames.slice(0, 5),
      },
      null,
      2,
    );
    const log = readDebugLogTail(home) ?? "(debug.log unavailable)";
    throw new Error(
      `waitForAcpReady phase 2 failed: ${err instanceof Error ? err.message : String(err)}\n` +
        `Diagnostic snapshot:\n${summary}\n` +
        `--- debug.log tail ---\n${log}\n--- end debug.log ---`,
      { cause: err },
    );
  }
}

/**
 * Poll `/acp/replay?since=0` until the serialized frame list
 * contains every needle in `needles` (default: `any` matches if at
 * least one needle is present; pass `mode: "all"` to require all).
 *
 * Replaces the `for (let attempt = 0; attempt < 30; attempt++) ... setTimeout(200)`
 * pattern that was duplicated across the structured view live specs. Hand-rolled
 * 30×200ms loops cap at a hard 6s deadline that races supervisor
 * handshakes under CI load; this helper uses `expect.poll` with
 * backoff intervals (100, 200, 500, 1000ms) and a 15s default timeout,
 * which both shortens the happy path and gives long-tail latencies
 * room to land. On timeout, expect.poll surfaces the last polled value
 * (the boolean) so failures stay readable.
 */
export async function waitForReplayContains(
  baseUrl: string,
  sessionId: string,
  needles: string | string[],
  options: { timeoutMs?: number; mode?: "any" | "all" } = {},
): Promise<void> {
  const list = Array.isArray(needles) ? needles : [needles];
  const mode = options.mode ?? "any";
  const timeout = options.timeoutMs ?? 15_000;
  await expect
    .poll(
      async () => {
        const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
        const frames: unknown[] = Array.isArray(replay) ? replay : (replay?.frames ?? []);
        const json = JSON.stringify(frames);
        return mode === "all" ? list.every((n) => json.includes(n)) : list.some((n) => json.includes(n));
      },
      { timeout, intervals: [100, 200, 500, 1000] },
    )
    .toBe(true);
}

/**
 * Enable the structured view supervisor for a session and wait for it to be
 * ready to accept prompts. Asserts the enable POST succeeded before
 * polling readiness so a 4xx / 5xx surfaces as an explicit failure
 * rather than a readiness timeout.
 */
export async function enableStructuredViewAndWait(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/enable`, {
    method: "POST",
  });
  if (!res.ok) {
    throw new Error(`structured view enable failed: status=${res.status} body=${await res.text()}`);
  }
  await waitForAcpReady(baseUrl, sessionId, timeoutMs, home);
}

/**
 * Wait for the structured view React surface to be mounted and interactive on
 * the current page. Resolves when the composer textarea is visible.
 *
 * `waitForAcpReady` checks the server-side supervisor handshake via
 * disk-backed replay; this checks the client side after `page.goto`. UI
 * story specs need both: the supervisor must be alive (otherwise prompt
 * sends drop), and the React tree must have mounted StructuredView so
 * clicks have something to land on.
 *
 * Default timeout is 15s, matching `waitForAcpReady`; the textbox
 * appears within a few hundred ms on the local happy path. Lazy-loaded
 * structured view chunks (`App.tsx` dynamic `import("./components/acp/StructuredView")`)
 * may add a short delay on first navigation.
 */
export async function waitForStructuredView(page: Page, timeoutMs = 15_000): Promise<void> {
  await expect(
    page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    }),
  ).toBeVisible({ timeout: timeoutMs });
}

/**
 * Resolve the `<select>` rendered by a FormFields `SelectField` whose
 * label text matches the given string. SelectField wraps `<label>` and
 * `<select>` as siblings inside a single `<div>`; the cheapest
 * unambiguous selector walks from the label up to that wrapper.
 */
export function settingsSelectByLabel(page: Page, labelText: string): Locator {
  return page.locator("label").filter({ hasText: labelText }).locator("xpath=..").locator("select").first();
}

/**
 * Resolve the `<input type="number">` rendered by NumberField whose
 * label text matches the given string. Same structure as
 * settingsSelectByLabel.
 */
export function settingsNumberInputByLabel(page: Page, labelText: string): Locator {
  return page
    .locator("label")
    .filter({ hasText: labelText })
    .locator("xpath=..")
    .locator('input[type="number"]')
    .first();
}

/** Click a top-level SettingsView tab by its visible label. */
export async function openSettingsTab(page: Page, label: string): Promise<void> {
  await page.getByRole("button", { name: label, exact: true }).click();
}

/**
 * Wait for SettingsView's mount-time async chain to settle before
 * interacting with its inputs. The component renders with seed state
 * `selectedProfile = "default"` and `settings = null`, then fires
 * `fetchProfiles()` + `loadSettings()` from a useEffect; when
 * `fetchProfiles` resolves it may flip `selectedProfile` to the real
 * default profile (e.g. "main"), which retriggers
 * `loadSettings(<new profile>)`. A test that calls
 * `selectOption(...)` between the optimistic local update and the
 * second `setSettings(...)` from the refire sees its choice
 * clobbered by the refire and the assertion sticks at the original
 * value (observed in settings-tmux-* specs on slower CI runners).
 *
 * The Profile selector lives in the top-level header inside
 * SettingsView. Waiting for it to expose a non-empty value means
 * `fetchProfiles` has resolved, `setSelectedProfile(real)` has
 * applied, and the subsequent `loadSettings(real)` has had a chance
 * to populate `settings`. Cheaper than polling `/api/profiles`
 * directly and avoids depending on an internal "loading" flag.
 */
export async function waitForSettingsLoaded(page: Page): Promise<void> {
  const profileSelect = settingsSelectByLabel(page, "Profile");
  await expect(profileSelect).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(async () => (await profileSelect.inputValue()).length, {
      timeout: 10_000,
    })
    .toBeGreaterThan(0);
}

/**
 * Attach diagnostic logs (daemon debug.log + fake-acp.log) to the
 * Playwright test report when the test failed. Skips attaching on
 * success to keep the report small.
 *
 * Call in the spec's `finally` block, BEFORE `serve.stop()` so the
 * isolated $HOME tree still exists. The "Structured view worker stopped"
 * banner that ate #1383 round 4 is a downstream symptom; the actual
 * cause (fake-ACP EPIPE, runner SIGTERM, daemon reap, etc.) shows
 * up in those two logs but vanishes with the home dir.
 */
export async function attachServeDiagnostics(testInfo: TestInfo, serve: { home: string }): Promise<void> {
  // Always attach. Both `testInfo.status` and `testInfo.errors` are
  // populated only AFTER the test function returns and all hooks run;
  // they are empty/undefined when called from the test body's
  // `finally`. The cost of always attaching (~few KB on success) is
  // worth the certainty that diagnostics actually fire on failure.
  // Playwright's HTML reporter inlines attachments on failed tests
  // and elides them on passes anyway, so the report size impact is
  // bounded.
  // App dir resolution mirrors aoeServe.ts#appDirFor: Linux uses
  // $XDG_CONFIG_HOME (which the harness sets to `${home}/config`),
  // macOS/Windows use `${home}/.agent-of-empires-dev`. Probe both so
  // the diagnostic works on either runner OS without plumbing the
  // platform through.
  const candidates = [
    join(serve.home, "config", "agent-of-empires-dev", "debug.log"),
    join(serve.home, ".agent-of-empires-dev", "debug.log"),
  ];
  const debugLog = candidates.find((p) => existsSync(p));
  if (debugLog) {
    try {
      const content = readFileSync(debugLog, "utf8");
      const tail =
        content.length > 64_000 ? `... (${content.length - 64_000} bytes elided)\n` + content.slice(-64_000) : content;
      await testInfo.attach("debug.log", {
        body: tail,
        contentType: "text/plain",
      });
    } catch (e) {
      await testInfo.attach("debug.log.read-error", {
        body: String(e),
        contentType: "text/plain",
      });
    }
  } else {
    await testInfo.attach("debug.log.missing", {
      body: `tried: ${candidates.join("\n       ")}`,
      contentType: "text/plain",
    });
  }
  const fakeLog = join(serve.home, "fake-acp.log");
  if (existsSync(fakeLog)) {
    try {
      const content = readFileSync(fakeLog, "utf8");
      await testInfo.attach("fake-acp.log", {
        body: content,
        contentType: "text/plain",
      });
    } catch (e) {
      await testInfo.attach("fake-acp.log.read-error", {
        body: String(e),
        contentType: "text/plain",
      });
    }
  } else {
    await testInfo.attach("fake-acp.log.missing", {
      body: `expected at ${fakeLog}`,
      contentType: "text/plain",
    });
  }
}

/**
 * Locator scoped to the active SessionWizard dialog. The wizard renders
 * a fixed-position overlay that visually covers the sidebar; without
 * this scope, `getByRole`/`getByText` queries can match background
 * sidebar / topbar elements behind the overlay.
 */
export function wizardScope(page: Page): Locator {
  return page.locator('[data-testid="session-wizard"]');
}

/**
 * Resolve the first text input that sits in the same wrapper `<div>`
 * as a `<label>` matching the given text. Works for both the wizard's
 * SessionStep field layout and FormFields TextField.
 */
export function inputByLabel(page: Page, labelText: string): Locator {
  return page.locator("label").filter({ hasText: labelText }).locator("xpath=..").locator('input[type="text"]').first();
}
