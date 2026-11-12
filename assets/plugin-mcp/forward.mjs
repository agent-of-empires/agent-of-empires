// Bridges a stdio MCP client to one AoE plugin MCP endpoint over the daemon's unix socket.
// Usage: node forward.mjs <socket> <plugin id> <server>
// Env: AOE_PLUGIN_MCP_TOKEN (the derived credential), AOE_PLUGIN_MCP_SESSION (the session id).
// Stdout carries only JSON-RPC lines; the credential is never printed.
import http from "node:http";
import readline from "node:readline";

const [socketPath, plugin, server] = process.argv.slice(2);
const token = process.env.AOE_PLUGIN_MCP_TOKEN ?? "";
const session = process.env.AOE_PLUGIN_MCP_SESSION ?? "";
const endpoint = `/mcp/plugins/${plugin}/${server}`;
// The socket router accepts only this Host.
const HOST = "aoe-plugin-mcp.invalid";
// Above the daemon's 600 s ceiling for one forwarded call.
const TIMEOUT_MS = 610_000;
const EXIT_GRACE_MS = 2_000;
const META_VERSION = "io.modelcontextprotocol/protocolVersion";
// From this revision on the server checks Mcp-Method and Mcp-Name against the body.
const STANDARD_HEADERS = "2026-07-28";
const NAME_PARAM = {
  "tools/call": "name",
  "prompts/get": "name",
  "resources/read": "uri",
  "resources/subscribe": "uri",
  "resources/unsubscribe": "uri",
  "tasks/get": "taskId",
  "tasks/update": "taskId",
  "tasks/cancel": "taskId",
};

let negotiated;
let stdinClosed = false;
const inFlight = new Map();
let pending = 0;

process.stdout.on("error", () => process.exit(0));

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function isRequest(message) {
  return typeof message?.method === "string" && message.id !== undefined && message.id !== null;
}

function fail(message, code, text) {
  if (isRequest(message)) {
    write({ jsonrpc: "2.0", id: message.id, error: { code, message: text } });
  }
}

// SEP-2243 header values travel Base64-wrapped when they are not plain visible ASCII.
function headerValue(value) {
  const plain =
    /^[\x20-\x7e]*$/.test(value) &&
    !/^[ \t]|[ \t]$/.test(value) &&
    !(value.startsWith("=?base64?") && value.endsWith("?="));
  return plain ? value : `=?base64?${Buffer.from(value, "utf8").toString("base64")}?=`;
}

function headersFor(message, body) {
  const headers = {
    host: HOST,
    authorization: `Bearer ${token}`,
    "x-aoe-session": session,
    accept: "application/json, text/event-stream",
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
  };
  const meta = message?.params?._meta?.[META_VERSION];
  const version = typeof meta === "string" ? meta : message?.method === "initialize" ? undefined : negotiated;
  if (typeof version !== "string") {
    return headers;
  }
  headers["mcp-protocol-version"] = version;
  if (version >= STANDARD_HEADERS && typeof message.method === "string") {
    headers["mcp-method"] = message.method;
    const name = message.params?.[NAME_PARAM[message.method]];
    if (typeof name === "string") {
      headers["mcp-name"] = headerValue(name);
    }
  }
  return headers;
}

function isJsonRpc(value) {
  return value !== null && typeof value === "object" && value.jsonrpc === "2.0";
}

// The endpoint answers plain JSON; an SSE-framed reply is unwrapped event by event.
function replies(contentType, body) {
  if (contentType.includes("text/event-stream")) {
    return body
      .split(/\r?\n\r?\n/)
      .map((event) =>
        event
          .split(/\r?\n/)
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).replace(/^ /, ""))
          .join("\n"),
      )
      .filter((data) => data.length > 0)
      .map((data) => JSON.parse(data));
  }
  const value = JSON.parse(body);
  return Array.isArray(value) ? value : [value];
}

function settle() {
  pending -= 1;
  if (stdinClosed && pending === 0) {
    process.exit(0);
  }
}

function forward(message, body) {
  pending += 1;
  let settled = false;
  const finish = () => {
    if (!settled) {
      settled = true;
      if (isRequest(message)) {
        inFlight.delete(message.id);
      }
      settle();
    }
  };
  const request = http.request(
    { socketPath, path: endpoint, method: "POST", headers: headersFor(message, body), timeout: TIMEOUT_MS },
    (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("error", () => {});
      response.on("close", () => {
        if (!settled && !request.cancelledByClient) {
          fail(message, -32603, "The AoE plugin MCP endpoint closed the reply early.");
        }
        finish();
      });
      response.on("end", () => {
        const text = Buffer.concat(chunks).toString("utf8");
        let messages = [];
        try {
          messages = text.length > 0 ? replies(response.headers["content-type"] ?? "", text) : [];
        } catch {
          messages = [];
        }
        const rpc = messages.filter(isJsonRpc);
        for (const reply of rpc) {
          if (message.method === "initialize" && typeof reply.result?.protocolVersion === "string") {
            negotiated = reply.result.protocolVersion;
          }
          write(reply);
        }
        if (rpc.length === 0) {
          fail(message, -32603, `The AoE plugin MCP endpoint answered HTTP ${response.statusCode} without a JSON-RPC reply.`);
        }
        finish();
      });
    },
  );
  if (isRequest(message)) {
    inFlight.set(message.id, request);
  }
  request.on("timeout", () => request.destroy(new Error("timeout")));
  request.on("error", (error) => {
    if (!settled && !request.cancelledByClient) {
      const cause = error.message === "timeout" ? "timed out" : `is unreachable (${error.code ?? "error"})`;
      fail(message, -32603, `The AoE plugin MCP socket ${cause}.`);
    }
    finish();
  });
  request.end(body);
}

// A cancelled request is abandoned: the endpoint stops the worker call when the connection drops.
function cancel(message) {
  if (message?.method !== "notifications/cancelled") {
    return;
  }
  const request = inFlight.get(message.params?.requestId);
  if (request) {
    request.cancelledByClient = true;
    request.destroy();
  }
}

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  if (line.trim().length === 0) {
    return;
  }
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    write({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
    return;
  }
  // One message per line; MCP has no batches since 2025-06-18.
  if (message === null || typeof message !== "object" || Array.isArray(message)) {
    write({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "Invalid Request" } });
    return;
  }
  cancel(message);
  forward(message, line);
});
input.on("close", () => {
  stdinClosed = true;
  if (pending === 0) {
    process.exit(0);
  }
  setTimeout(() => process.exit(0), EXIT_GRACE_MS).unref();
});
