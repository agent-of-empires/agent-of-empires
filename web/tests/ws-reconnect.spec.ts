import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";

// Verifies useTerminal.ts reconnects after a WS drop and that the first
// retry fires within the expected fast-start window (200ms first delay).
// Guards against regressions of the backoff constants away from #1455's
// fast-start schedule back toward the old slow 1s exponential ladder.
//
// Note: we can't use installTerminalSpies here, because Playwright's
// page.routeWebSocket installs its own WebSocket proxy AFTER addInitScript
// runs, which overwrites any window.WebSocket patch. Instead we count
// connection attempts in the route handler (Node side) and assert on that.

async function mockApisExceptWs(page: Page, sessionTitle: string) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: sessionTitle,
            title: sessionTitle,
            project_path: `/tmp/${sessionTitle}`,
            group_path: "/tmp",
            tool: "claude",
            status: "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) =>
    r.fulfill({ json: { ok: true } }),
  );
  await page.route("**/api/sessions/*/terminal", (r) =>
    r.fulfill({ status: 200, body: "" }),
  );
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "docker/status" ? {} : [] }),
    );
  }
}

async function openSession(page: Page, title: string) {
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  await clickSidebarSession(page, title);
  await page
    .locator(".xterm")
    .first()
    .waitFor({ state: "visible", timeout: 10_000 });
}

test.describe("Terminal WebSocket reconnection", () => {
  test("reconnects after a dropped connection", async ({ page }) => {
    const title = "reconnect-test";
    await mockApisExceptWs(page, title);

    // Side-channel WS (shell host terminal, container ws): keep them open
    // and mute so they don't affect our main-terminal reconnect observations.
    await page.routeWebSocket(
      /\/sessions\/[^/]+\/(terminal\/ws|container-ws)$/,
      (ws) => {
        ws.onMessage(() => {});
      },
    );

    let attempts = 0;
    let firstClosedAt = 0;
    let secondOpenedAt = 0;
    await page.routeWebSocket(/\/sessions\/[^/]+\/ws$/, (ws) => {
      attempts += 1;
      const attemptNum = attempts;
      ws.onMessage(() => {});
      setTimeout(() => {
        try {
          ws.send(Buffer.from("$ "));
        } catch {
          /* may be closed */
        }
      }, 30);
      if (attemptNum === 1) {
        setTimeout(() => {
          firstClosedAt = Date.now();
          try {
            ws.close();
          } catch {
            /* already closed */
          }
        }, 150);
      } else if (attemptNum === 2) {
        secondOpenedAt = Date.now();
      }
    });

    await openSession(page, title);

    // Wait for the reconnect to fire. First retry is scheduled at 200ms
    // under the fast-start ladder; 5s upper bound still fails fast if we
    // regressed to a slow exponential delay.
    await expect
      .poll(() => attempts, { timeout: 5_000 })
      .toBeGreaterThanOrEqual(2);

    // Guard: both timestamps must have been set. Without this check, a 0
    // firstClosedAt would make elapsed comically large and the < 1500
    // assertion would dominate with a misleading message.
    expect(firstClosedAt).toBeGreaterThan(0);
    expect(secondOpenedAt).toBeGreaterThan(0);

    // First retry is scheduled at 200ms backoff. Allow 50-2500ms to
    // absorb Playwright/browser scheduling and WebGL warm-up jitter
    // under parallel CI load while still catching a regression to the
    // old 1s+ exponential first-retry delay (which produced 4s+ first
    // retries in measurement and was the user-visible #1455 symptom).
    const elapsed = secondOpenedAt - firstClosedAt;
    expect(elapsed).toBeGreaterThan(50);
    expect(elapsed).toBeLessThan(2_500);

    // Second connection should be stable, no further reconnects.
    await page.waitForTimeout(1_500);
    expect(attempts).toBe(2);
  });

  test("'online' event short-circuits the backoff and dials immediately", async ({
    page,
  }) => {
    // Drop attempts 1-4 so the next scheduled backoff is the longer 3s
    // delay (200+400+800+1500 = 2.9s of total wait under the fast-start
    // schedule). Dispatch a window 'online' event during that 3s window.
    // With the #1009 fix this triggers manualReconnect → connect()
    // immediately; without it, the listener never reconnects on a CLOSED
    // socket.
    const title = "online-test";
    await mockApisExceptWs(page, title);

    await page.routeWebSocket(
      /\/sessions\/[^/]+\/(terminal\/ws|container-ws)$/,
      (ws) => {
        ws.onMessage(() => {});
      },
    );

    let attempts = 0;
    const closeTimes: number[] = [];
    const openTimes: number[] = [];
    await page.routeWebSocket(/\/sessions\/[^/]+\/ws$/, (ws) => {
      attempts += 1;
      const attemptNum = attempts;
      openTimes.push(Date.now());
      ws.onMessage(() => {});
      if (attemptNum <= 4) {
        setTimeout(() => {
          closeTimes.push(Date.now());
          try {
            ws.close();
          } catch {
            /* already closed */
          }
        }, 50);
      }
    });

    await openSession(page, title);

    // Wait for four retry cycles to land. Budget: ~50+200+50+400+50+800+50+1500 ≈ 3.1s.
    await expect.poll(() => closeTimes.length, { timeout: 8_000 }).toBe(4);

    // Fire 'online' while the fifth backoff (3s) is still pending. Wait
    // a short moment so the listener is firmly armed and the WS is
    // CLOSED, then dispatch.
    await page.waitForTimeout(200);
    const beforeOnline = Date.now();
    await page.evaluate(() => window.dispatchEvent(new Event("online")));

    // Attempt 5 should arrive well under the 3s backoff that would
    // otherwise gate it.
    await expect
      .poll(() => attempts, { timeout: 2_000 })
      .toBeGreaterThanOrEqual(5);
    const fifthOpenedAt = openTimes[4];
    expect(fifthOpenedAt).toBeDefined();
    expect(fifthOpenedAt! - beforeOnline).toBeLessThan(1_500);
  });

  test("retries more than the old max of 3", async ({ page }) => {
    // The old hardcoded MAX_RETRIES was 3. The new value is 7 with the
    // fast-start ladder (200ms, 400ms, 800ms, ...). We don't wait the
    // full schedule; we just verify the counter climbs past the old limit
    // to prove the new constant is in effect. Budget: 200+400+800 = 1.4s
    // for 4 total attempts.
    const title = "retry-test";
    await mockApisExceptWs(page, title);

    await page.routeWebSocket(
      /\/sessions\/[^/]+\/(terminal\/ws|container-ws)$/,
      (ws) => {
        ws.onMessage(() => {});
      },
    );

    let attempts = 0;
    await page.routeWebSocket(/\/sessions\/[^/]+\/ws$/, (ws) => {
      attempts += 1;
      ws.onMessage(() => {});
      setTimeout(() => {
        try {
          ws.close();
        } catch {
          /* already closed */
        }
      }, 30);
    });

    await openSession(page, title);

    await expect
      .poll(() => attempts, { timeout: 5_000, intervals: [100, 250] })
      .toBeGreaterThanOrEqual(4);
  });

  test("switching sessions mid-retry does not resurrect the old session's socket", async ({
    page,
  }) => {
    // Regression for #1455. User story: a session drops its WS and the
    // client schedules a retry. Before the retry fires, the user clicks
    // a different session in the sidebar. The old session's onclose
    // closure must not be allowed to dial a ghost socket against the
    // OLD sessionId after the effect cleanup has run.
    const oldTitle = "old-session";
    const newTitle = "new-session";
    await page.route("**/api/login/status", (r) =>
      r.fulfill({ json: { required: false, authenticated: true } }),
    );
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") return r.fulfill({ status: 400 });
      return r.fulfill({
        json: {
          sessions: [oldTitle, newTitle].map((t) => ({
            id: t,
            title: t,
            project_path: `/tmp/${t}`,
            group_path: "/tmp",
            tool: "claude",
            status: "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
          })),
          workspace_ordering: [],
        },
      });
    });
    await page.route("**/api/sessions/*/ensure", (r) =>
      r.fulfill({ json: { ok: true } }),
    );
    await page.route("**/api/sessions/*/terminal", (r) =>
      r.fulfill({ status: 200, body: "" }),
    );
    await page.route("**/api/sessions/*/diff/files", (r) =>
      r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
    );
    for (const path of [
      "settings",
      "themes",
      "agents",
      "profiles",
      "groups",
      "devices",
      "docker/status",
      "about",
    ]) {
      await page.route(`**/api/${path}`, (r) =>
        r.fulfill({ json: path === "docker/status" ? {} : [] }),
      );
    }

    await page.routeWebSocket(
      /\/sessions\/[^/]+\/(terminal\/ws|container-ws)$/,
      (ws) => {
        ws.onMessage(() => {});
      },
    );

    // Tuple of (sessionId, dialedAtMs) per attempt so we can assert
    // "no NEW old-session attempts after the switch moment", independent
    // of how many retries the client legitimately fired before the user
    // clicked away.
    const dials: { id: string; at: number }[] = [];
    await page.routeWebSocket(/\/sessions\/[^/]+\/ws$/, (ws) => {
      const url = new URL(ws.url());
      const match = url.pathname.match(/\/sessions\/([^/]+)\/ws$/);
      const id = match?.[1] ?? "";
      dials.push({ id, at: Date.now() });
      ws.onMessage(() => {});
      if (id === oldTitle) {
        // Drop old-session attempts fast so the client keeps scheduling
        // retries. Without the cleanup fix, the OLD socket's onclose
        // closure fires after the session-switch cleanup runs and
        // schedules a setTimeout that calls connect() against the OLD
        // sessionId, producing ghost dials we detect with `switchAt`.
        setTimeout(() => {
          try {
            ws.close({ code: 1011, reason: "openpty_failed" });
          } catch {
            /* already closed */
          }
        }, 30);
      } else {
        setTimeout(() => {
          try {
            ws.send(Buffer.from("new$ "));
          } catch {
            /* may be closed */
          }
        }, 20);
      }
    });

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await clickSidebarSession(page, oldTitle);
    await page
      .locator(".xterm")
      .first()
      .waitFor({ state: "visible", timeout: 10_000 });

    // Wait for at least one old-session attempt to land.
    await expect
      .poll(() => dials.filter((d) => d.id === oldTitle).length, {
        timeout: 3_000,
      })
      .toBeGreaterThanOrEqual(1);

    // Switch sessions, then wait for the new-session WS to dial. That
    // proves the React commit phase ran (cleanup + new-effect ordered
    // synchronously inside commit), so any subsequent old-session dial
    // is unambiguously a ghost.
    await clickSidebarSession(page, newTitle);
    await expect
      .poll(() => dials.some((d) => d.id === newTitle), { timeout: 5_000 })
      .toBe(true);

    const oldCountAtSwitch = dials.filter((d) => d.id === oldTitle).length;

    // Wait long enough to absorb every fast-start retry the OLD effect
    // could have scheduled before cleanup (worst case 200+400+800+1500
    // = 2.9s).
    await page.waitForTimeout(3_000);

    const oldCountFinal = dials.filter((d) => d.id === oldTitle).length;
    expect(oldCountFinal).toBe(oldCountAtSwitch);
  });
});
