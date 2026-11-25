# Recording techniques

Patterns for behaviors that a plain click-and-type script cannot show. Each one keeps the input real, or names the single piece that is simulated.

## Move to an element, then act

Clips read best when the pointer travels visibly to its target before acting:

```js
const go = async (loc, steps = 18) => {
  const b = await loc.boundingBox();
  const [x, y] = [b.x + Math.min(b.width / 2, 180), b.y + b.height / 2];
  await glide(page, x, y, steps);
  await sleep(350);
  return [x, y];
};
```

## Prove a link opens the right target

Headless recording draws no OS cursor, so set `window.__demoLinkTip = true` after `goto`: the pointer dot turns blue over `cursor: pointer` and labels the href under it. Click with the real mouse, then show the URL the new tab actually loaded:

```js
const popup = context.waitForEvent("page", { timeout: 8000 });
await page.mouse.click(x, y);
const tab = await popup;
await tab.waitForURL((u) => u.protocol.startsWith("http"), { timeout: 8000 }).catch(() => {});
const url = tab.url();
await tab.close();
await page.bringToFront();
await caption(page, `Opened: ${url}`, { top: true });
```

Terminal links render as `a[href]` rows in the live terminal, so `page.locator('a[href*="<unique part>"]')` returns one element per wrapped row; hover each in turn.

## Native `<select>`

Headless Chromium draws no popup for a native select. Expand it in place so its real options and optgroups render, click the real option, then collapse it, and mention this in the PR comment:

```js
await page.evaluate((id) => { const s = document.getElementById(id); s.size = s.querySelectorAll("option,optgroup").length; }, "terminal-font-family");
// glide to and click sel.locator("option", { hasText: label })
await page.evaluate((id) => { document.getElementById(id).size = 0; }, "terminal-font-family");
```

## Touch gestures

`launch(name, { mobile: true })` gives an iPhone 13 viewport with touch. Drive gestures through CDP so the page receives real touch and pointer events:

```js
const cdp = await context.newCDPSession(page);
const touch = (type, touchPoints) => cdp.send("Input.dispatchTouchEvent", { type, touchPoints });
await touch("touchStart", [{ x, y, id: 1 }]);
for (let i = 1; i <= 4; i++) { await touch("touchMove", [{ x, y: y + (dy * i) / 4, id: 1 }]); await sleep(16); }
await touch("touchEnd", []);
```

## Simulate a lost event

Chromium cannot drop a real event the way iOS sometimes drops a touch end. Swallow it in the capture phase before the page sees it, toggle the flag around the one gesture, and say in the PR comment that this input is simulated:

```js
await context.addInitScript(() => {
  window.__dropEnds = false;
  for (const t of ["pointerup", "pointercancel", "lostpointercapture", "touchend", "touchcancel"])
    addEventListener(t, (e) => { if (window.__dropEnds) e.stopImmediatePropagation(); }, true);
});
```

## Show what reaches the daemon

To prove input stops or starts (a repeat that ends, keys that are sent), count it at the WebSocket and render the count on screen:

```js
await context.addInitScript(() => {
  window.__sent = 0;
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function (d) {
    const s = typeof d === "string" ? d : new TextDecoder().decode(d);
    window.__sent += (s.match(/\x1b\[[ABCD]/g) || []).length;
    return send.call(this, d);
  };
});
```

Read `window.__sent` before and after a pause to assert the behavior in the script log as well as on screen.

## Seed state through the dashboard

Use `prep()` to write state the clip depends on through the dashboard's own API before recording starts, for example a setting saved from "another device":

```js
await prep(async () => {
  const s = JSON.parse(localStorage.getItem("aoe-web-settings") || "{}");
  s.terminalFontFamily = "SF Mono";
  return (await fetch("/api/app-state/web-ui-state", {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ "aoe-web-settings": JSON.stringify(s) }),
  })).status;
});
```

## TUI clips (ttyd)

Each browser connection to ttyd starts a fresh `aoe`, so everything the TUI replays on start shows in the clip.

- Install with `brew install ttyd` (`/opt/homebrew/bin/ttyd` on macOS). Start it from the shell that exported the isolated env (`XDG_CONFIG_HOME`, `AOE_TMUX_SOCKET`, `CLAUDE_CONFIG_DIR`): it passes its environment to `aoe`. On macOS set `LANG=en_US.UTF-8` so glyphs render. `-t rendererType=dom` makes the screen text, so scripts can assert on it. `launch()` uses a 1280x800 viewport; `fontSize=15` shows Home with its preview at that size.
- Replayed notices show in the clip. If the Welcome walkthrough shows, press Escape once in an unrecorded connection. "Prompt not delivered" modals replay once per old session that missed its prompt: connect unrecorded and press Enter until none show, and have the clip script dismiss any left before its first caption. To remove the old sessions, first check `echo $XDG_CONFIG_HOME` and that `aoe list` shows only scene sessions, then `aoe rm --purge <id>`; add `--delete-worktree` to also remove a managed worktree, which is kept by default.
- Keys go where the selection is. Enter on a session row attaches it, and keys then reach the agent. Move the selection first (`End` selects the last row in your scene, such as Suggested) and check `innerText` after each key instead of sleeping.
- Claude Code's own terminal dialogs (folder trust, project MCP approval) can block a typed prompt. Use a Structured view for the start: the kind's preset View row, or `aoe add --structured-view`.
- Captions cover the key-hint footer; pass `{ top: true }` when a step needs it.
- Stop scratch processes by the PID you saved (`$DEMO/ttyd.pid`) or `aoe serve --stop`. Never `pkill -f` by name: it also kills your real sessions.
