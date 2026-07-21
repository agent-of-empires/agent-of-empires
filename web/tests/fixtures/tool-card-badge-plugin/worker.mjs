// Worker for the fake provenance plugin used by the live tool-card-badge test
// (#2986). The plugin worker is the JSON-RPC client: it writes one request per
// line to stdout and reads one response per line on stdin (see
// src/plugin/protocol.rs). It lists sessions and pushes a tool-card-badge entry
// targeting the `acmecorp` MCP server, matching the tool call the fake ACP agent
// emits. It loops forever (re-pushing for any new session) so the host does not
// clear its ui-state on exit.

import { createInterface } from "node:readline";

let nextId = 1;
const pending = new Map();

createInterface({ input: process.stdin }).on("line", (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  // Only responses to our own requests matter; ignore host-initiated
  // notifications (e.g. composer-action forwards) this plugin does not use.
  if (msg.id != null && pending.has(msg.id)) {
    const resolve = pending.get(msg.id);
    pending.delete(msg.id);
    resolve(msg);
  }
});

function call(method, params) {
  return new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
}

async function pushBadges() {
  const res = await call("sessions.list", {});
  const sessions = res.result?.sessions ?? [];
  for (const s of sessions) {
    await call("ui.state.set", {
      slot: "tool-card-badge",
      id: "provenance",
      session_id: s.id,
      payload: {
        items: [{ target: { kind: "mcp", name: "acmecorp" }, text: "Company MCP", tone: "info" }],
      },
    });
  }
}

for (;;) {
  try {
    await pushBadges();
  } catch {
    // Transient; retry on the next tick.
  }
  await new Promise((r) => setTimeout(r, 1000));
}
