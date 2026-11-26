#!/usr/bin/env node
/**
 * Fake plugin proxy for tests: `fake-proxy [flags] -- <adapter argv...>`.
 * Spawns the adapter with piped stdio, forwards both directions, and adds
 * `_meta.aoe.proxy` to the initialize result. Inherited fds and env pass through.
 * FAKE_PROXY_RECORD_DIR: writes argv.json (own argv), pid, and env.json
 * (AOE_PROXY_LAUNCH_CONFIG as received) there.
 * FAKE_PROXY_MODE: "bad-hello" (wrong plugin_id), "no-hello" (no hello), "die"
 * (exit 1 before spawning the adapter); default is a valid hello.
 */
import { spawn } from "node:child_process";
import { fstatSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";

const argv = process.argv.slice(2);
const dir = process.env.FAKE_PROXY_RECORD_DIR;
if (dir) {
  writeFileSync(join(dir, "argv.json"), JSON.stringify(argv));
  writeFileSync(join(dir, "pid"), String(process.pid));
  writeFileSync(
    join(dir, "env.json"),
    JSON.stringify({
      AOE_PROXY_LAUNCH_CONFIG: process.env.AOE_PROXY_LAUNCH_CONFIG ?? null,
    }),
  );
}

const mode = process.env.FAKE_PROXY_MODE ?? "ok";
if (mode === "die") process.exit(1);

const sep = argv.indexOf("--");
if (sep < 0) {
  console.error("fake-proxy: missing -- separator");
  process.exit(2);
}
const [command, ...args] = argv.slice(sep + 1);
// Pass every open fd above 2 through (the sandbox env fd rides one).
const extraFds = [];
for (let fd = 3; fd < 16; fd++) {
  try {
    fstatSync(fd);
    extraFds.push(fd);
  } catch {
    // Not open.
  }
}
const child = spawn(command, args, {
  stdio: ["pipe", "pipe", "inherit", ...extraFds],
});

const hello = {
  plugin_id: mode === "bad-hello" ? "wrong-plugin" : "fake-proxy",
  protocol_version: 1,
  primitive_schema_version: 1,
};

process.stdin.pipe(child.stdin);
const lines = createInterface({ input: child.stdout });
lines.on("line", (line) => {
  let out = line;
  if (mode !== "no-hello") {
    try {
      const msg = JSON.parse(line);
      if (msg.result?.protocolVersion !== undefined) {
        msg.result._meta = {
          ...msg.result._meta,
          aoe: { ...msg.result._meta?.aoe, proxy: hello },
        };
        out = JSON.stringify(msg);
      }
    } catch {
      // Not JSON; forward untouched.
    }
  }
  process.stdout.write(out + "\n");
});
child.on("exit", (code, signal) => process.exit(code ?? (signal ? 1 : 0)));
for (const sig of ["SIGTERM", "SIGINT"]) process.on(sig, () => child.kill(sig));
