import type { Page } from "@playwright/test";

// Shared mocks so a running `aoe serve` + tmux aren't required. We stub the
// REST API and route the PTY WebSocket so the xterm.js terminal mounts and the
// gesture handlers in useTerminal.ts are exercised against the real frontend.

export interface MockHandle {
  /** Raw bytes received from the page via WebSocket (PTY data + JSON messages). */
  wsMessages: Buffer[];
}

export async function mockTerminalApis(page: Page): Promise<MockHandle> {
  const handle: MockHandle = { wsMessages: [] };
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: "pinch-test",
            title: "pinch-test",
            project_path: "/tmp/pinch-test",
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
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
  );
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
  await page.routeWebSocket(/\/sessions\/.*\/(ws|container-ws)$/, (ws) => {
    ws.onMessage((msg) => {
      if (Buffer.isBuffer(msg)) handle.wsMessages.push(msg);
      else handle.wsMessages.push(Buffer.from(msg));
    });
    setTimeout(() => {
      try {
        ws.send(Buffer.from("$ "));
      } catch {
        // ws may have been closed while the test ended — safe to ignore
      }
    }, 50);
  });
  return handle;
}

// Install a WebSocket constructor spy and a localStorage.setItem spy on
// window. Both run before any frontend script, so the React app sees the
// patched globals. The counts let tests prove that a setting change does
// NOT reopen the PTY, and that a gesture that should be a no-op did not
// write to localStorage.
export async function installTerminalSpies(page: Page) {
  await page.addInitScript(() => {
    const Orig = window.WebSocket;
    (window as unknown as { __WS_COUNT__: number }).__WS_COUNT__ = 0;
    // Preserve name + prototype by extending
    window.WebSocket = class extends Orig {
      constructor(url: string | URL, protocols?: string | string[]) {
        super(url, protocols);
        (window as unknown as { __WS_COUNT__: number }).__WS_COUNT__ += 1;
      }
    } as typeof WebSocket;

    (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__ = [];
    const origSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key: string, value: string) {
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__.push(`${key}=${value}`);
      return origSetItem.call(this, key, value);
    };
  });
}

export function readFontSize(page: Page, which: "mobile" | "desktop") {
  return page.evaluate((which) => {
    const raw = localStorage.getItem("aoe-web-settings");
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    return which === "mobile" ? parsed.mobileFontSize : parsed.desktopFontSize;
  }, which);
}

export async function seedSettings(page: Page, settings: { mobileFontSize?: number; desktopFontSize?: number }) {
  await page.evaluate((settings) => {
    localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({
        mobileFontSize: 8,
        desktopFontSize: 14,
        autoOpenKeyboard: true,
        ...settings,
      }),
    );
  }, settings);
}

// Synthesize a multi-touch TouchEvent on the .xterm element.
// Playwright's page.touchscreen is single-finger only; building raw Touch
// objects is the only cross-browser way to dispatch two-finger gestures.
export async function fireTouches(
  page: Page,
  type: "touchstart" | "touchmove" | "touchend" | "touchcancel",
  points: { x: number; y: number }[],
) {
  await page.evaluate(
    ({ type, points }) => {
      const target = document.querySelector<HTMLElement>(".xterm");
      if (!target) throw new Error(".xterm not mounted");
      const rect = target.getBoundingClientRect();
      const touches = points.map((p, i) => {
        const clientX = rect.left + p.x;
        const clientY = rect.top + p.y;
        return new Touch({
          identifier: i,
          target,
          clientX,
          clientY,
          pageX: clientX,
          pageY: clientY,
          screenX: clientX,
          screenY: clientY,
          radiusX: 2,
          radiusY: 2,
          rotationAngle: 0,
          force: 1,
        });
      });
      const lifted = type === "touchend" || type === "touchcancel";
      const ev = new TouchEvent(type, {
        bubbles: true,
        cancelable: true,
        touches: lifted ? [] : touches,
        targetTouches: lifted ? [] : touches,
        changedTouches: touches,
      });
      target.dispatchEvent(ev);
    },
    { type, points },
  );
}
