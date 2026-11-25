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
