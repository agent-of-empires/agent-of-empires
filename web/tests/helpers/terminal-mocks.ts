import type { Page } from "@playwright/test";

// Shared mocks so a running `aoe serve` + tmux aren't required. We stub the
// REST API and route the PTY WebSocket so the xterm.js terminal mounts and the
// gesture handlers in useTerminal.ts are exercised against the real frontend.

export interface MockHandle {
  /** Raw bytes received from the page via WebSocket (PTY data + JSON messages). */
  wsMessages: Buffer[];
  /** Messages the page sent on the capture-snapshot live-ws route
   *  (mobile live view): binary input bytes + JSON control messages. */
  liveMessages: Buffer[];
  /** Push a live frame to every connected live-ws client. */
  pushLiveFrame: (frame: {
    content: string;
    rows: number;
    history: number;
    cursor?: { x: number; y: number } | null;
  }) => void;
}

/** Build a deterministic live frame: `history` numbered scrollback lines
 *  followed by a `rows`-tall screen with a prompt on its first line. */
export function makeLiveFrame(opts: { rows?: number; history?: number; window?: number } = {}) {
  const rows = opts.rows ?? 24;
  const history = opts.history ?? 0;
  const window = Math.min(opts.window ?? rows, rows + history);
  const fetchedHistory = Math.max(0, window - rows);
  const lines: string[] = [];
  for (let i = history - fetchedHistory + 1; i <= history; i++) {
    lines.push(`history line ${String(i).padStart(3, "0")} lorem ipsum`);
  }
  lines.push("$ ready");
  for (let i = 1; i < rows; i++) lines.push("");
  return {
    content: lines.join("\n") + "\n",
    rows,
    history,
    cursor: { x: 8, y: 0 },
  };
}

export async function mockTerminalApis(page: Page, opts: { liveHistory?: number } = {}): Promise<MockHandle> {
  const liveSockets: Array<{ send: (data: string) => void }> = [];
  const handle: MockHandle = {
    wsMessages: [],
    liveMessages: [],
    pushLiveFrame: (frame) => {
      const payload = JSON.stringify({ type: "frame", cursor: null, ...frame });
      for (const ws of liveSockets) {
        try {
          ws.send(payload);
        } catch {
          // closed socket at test teardown; ignore
        }
      }
    },
  };
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
  // Matches the bare path plus the `?index=N` query (#2437) for POST ensure and
  // DELETE kill, and the container-terminal variant.
  await page.route("**/api/sessions/*/terminal*", (r) => r.fulfill({ status: 200, body: "" }));
  await page.route("**/api/sessions/*/container-terminal*", (r) => r.fulfill({ status: 200, body: "" }));
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
  // Capture-snapshot live view (mobile). Replies to resize/window control
  // messages with a frame sized accordingly so the component always has
  // content to render, mirroring src/server/live_ws.rs.
  await page.routeWebSocket(/\/sessions\/.*\/live-ws(\?.*)?$/, (ws) => {
    liveSockets.push(ws);
    let rows = 24;
    let window = 24;
    const history = opts.liveHistory ?? 120;
    const reply = () => {
      try {
        ws.send(JSON.stringify({ type: "frame", ...makeLiveFrame({ rows, history, window }) }));
      } catch {
        // closed socket at test teardown; ignore
      }
    };
    ws.onMessage((msg) => {
      if (Buffer.isBuffer(msg)) {
        handle.liveMessages.push(msg);
        return;
      }
      handle.liveMessages.push(Buffer.from(msg));
      try {
        const control = JSON.parse(String(msg)) as { type?: string; rows?: number; lines?: number };
        if (control.type === "resize" && control.rows) {
          rows = control.rows;
          window = Math.max(window, rows);
          reply();
        } else if (control.type === "window" && control.lines) {
          window = control.lines;
          reply();
        }
      } catch {
        // non-JSON text; ignore
      }
    });
    setTimeout(reply, 50);
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

export async function seedSettings(
  page: Page,
  settings: { mobileFontSize?: number; desktopFontSize?: number; autoOpenKeyboard?: boolean },
) {
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
      const target = document.querySelector<HTMLElement>("[data-live-terminal] > div, .xterm");
      if (!target) throw new Error("no terminal surface mounted");
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
