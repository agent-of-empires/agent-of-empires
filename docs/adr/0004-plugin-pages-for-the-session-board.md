# The Session Board is a plugin; core gains sandboxed plugin pages

Status: proposed. Decides #4433. Terms follow `CONTEXT.md`: **Board**, **Column**, **Card**, **Stage**, **Signal**, **Rule**; a **Plugin** contributes **UI slots**.

Issue #3446 settled on a plugin-first Board, and the read-only `kanban-lite` prototype (#3456) stalled because plugins had no page surface, no way to write per-session state from the browser, and a worker cannot read other plugins' slot values. We considered a native core Board and rejected it: the Board serves a subset of users and would add product surface to core that maintainers do not want to own. Instead core grows one general capability, a `page` UI slot: a plugin ships a single self-contained HTML file, the Web dashboard renders it in a sandboxed iframe, and a versioned `postMessage` bridge exposes a capability-gated API: the session list, the aggregated UI state of every installed plugin exactly as `GET /api/plugins/ui-state` already returns it to any authenticated client (no new cross-plugin state API, not the event log), the plugin's own actions, open-session navigation, archive and trash under `session.write`, and theme variables. The Board (`agent-of-empires.kanban`) is the first consumer.

## Loading and confinement

- The frame is `sandbox="allow-scripts"` and nothing else: no `allow-same-origin`, so the document has an opaque origin with no access to the dashboard's storage, cookies or token; no `allow-forms`, `allow-popups` or `allow-top-navigation`, so it cannot submit, open windows or navigate the dashboard.
- The bundle is not loaded through `srcdoc`: a `srcdoc` document inherits the dashboard policy, whose `script-src 'self'` would refuse the bundle's inline script, and a policy inside the frame cannot lift an inherited one. Instead the Daemon serves the bundle at a dedicated authenticated route and the iframe's `src` points there, so the response carries its own policy. The frame request is initiated by the dashboard and is same-site, so cookie authentication applies; how the route authenticates when device binding is on is settled in #4460.
- That route's policy is no-egress: `default-src 'none'`, inline script and style allowed (the bundle is the only script and nothing else can be loaded), `img-src data: blob:`, `connect-src 'none'`, `form-action 'none'`, `base-uri 'none'`, `frame-ancestors 'self'`, with the frame-denying headers relaxed to same-origin on this route only. It blocks every external resource load and connection, including the GitHub image hosts and `ws:`/`wss:` the dashboard policy permits.
- Sandbox and policy together cover resource loads, connections, forms, popups and navigation. They do not prove the absence of every exfiltration path (timing side channels, content the user copies out); the bridge stays the only sanctioned data path and its surface stays minimal.

## Consequences

- Plugin JavaScript never runs on the dashboard origin and never sees the access token; everything it can do is enumerated by the bridge and gated by manifest capabilities.
- Writes from a page are fire-and-forget through the existing plugin `action` endpoint; results surface through the UI-state poll. A request/response channel is deferred until a plugin needs it.
- Per-session plugin state lives in `session.meta`; plugin-wide state in `plugin.storage`. Core gains no Board-specific schema or migration.
- Confirms direction A of #3446 with the extension points it anticipated.
