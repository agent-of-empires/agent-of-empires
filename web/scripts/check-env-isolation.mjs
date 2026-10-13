#!/usr/bin/env node
// Static production-env inventory. Behavioral isolation stays in Vitest.
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, basename, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const helper = join(web, "tests/helpers/isolatedEnv.ts");
const compiled = ts.transpileModule(readFileSync(helper, "utf8"), {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const { isolateEnv, INHERITED_PATH_VARS, HOST_STATE_VARS } = await import(
  "data:text/javascript;base64," + Buffer.from(compiled).toString("base64")
);
const cases = ts.createSourceFile(
  "isolatedEnv.test.ts",
  readFileSync(join(web, "tests/helpers/isolatedEnv.test.ts"), "utf8"),
  ts.ScriptTarget.Latest,
  true,
);
const declarations = cases.statements
  .filter(ts.isVariableStatement)
  .flatMap((statement) => statement.declarationList.declarations);
const hostileCases = declarations.find((declaration) => declaration.name.getText(cases) === "NON_SUFFIX_HOST_STATE");
if (!hostileCases?.initializer || !ts.isObjectLiteralExpression(hostileCases.initializer))
  throw new Error("Missing hostile isolation cases");
const covered = new Set(hostileCases.initializer.properties.map((property) => property.name?.getText(cases)));
const uncovered = [...HOST_STATE_VARS].filter((name) => !covered.has(name));
if (uncovered.length) throw new Error("Host-state isolation has no behavioral hostile case: " + uncovered.join(", "));
/** Every src/ environment read must be neutralized or explicitly safe to inherit. */
const INHERITED_BY_CONTRACT = new Set([
  ...INHERITED_PATH_VARS,
  // systemd's id of the unit invocation; aoe only restores the caller's value
  // after `systemd-run` overwrites it and resolves nothing from it.
  "INVOCATION_ID",
  // Timing, tracing, and test switches. Their values are numbers or flags, so
  // a hostile one can slow a spec down but cannot point the daemon anywhere.
  "AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS",
  "AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT",
  "AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES",
  "AOE_ACP_TRACE",
  "AOE_ACP_WATCHDOG_POLL_MS",
  "AOE_E2E_DEBUG",
  "AOE_ENVGUARD_DUP",
  "AOE_ENVGUARD_EMPTY",
  "AOE_ENVGUARD_UNSET",
  "AOE_ENVGUARD_UNSET_ME",
  "AOE_FILE_WATCH",
  "AOE_LOG_LEVEL",
  "AOE_MOUSE_CAPTURE",
  "AOE_NO_LNAV_TIP",
  "AOE_PRIVDROP_TESTS",
  "AOE_RECOVERY_HOOK_TIMEOUT_MS",
  "AOE_RESUME_IDLE_CHECK_INTERVAL_MS",
  "AOE_RESUME_IDLE_GRACE_MS",
  "AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS",
  "AOE_SILENT_ORPHAN_FAST_GRACE_MS",
  "AOE_SILENT_ORPHAN_GRACE_MS",
  "AOE_TERMINAL_TRACE",
  "AOE_TEST_TOKEN_GRACE_SECS",
  "AOE_TEST_TOKEN_LIFETIME_SECS",
  // Reaches a real agent binary, never the daemon's own state, and every live
  // spec runs a fake agent shim.
  "CLAUDE_CODE_USE_VERTEX",
  // Terminal, desktop, and remote-session hints the daemon forwards into the
  // sessions it creates. `open_url` reads BROWSER and WSL_DISTRO_NAME to
  // decide whether a browser could reach the user at all; both are hints
  // about the machine the spec runs on, not state the daemon resolves
  // anything from, and `AOE_OPEN_URL_TO` is already dropped above.
  "BROWSER",
  "DISPLAY",
  "DO_NOT_TRACK",
  "MOSH_CONNECTION",
  "NO_COLOR",
  "SSH_CLIENT",
  "SSH_CONNECTION",
  "SSH_TTY",
  "USER",
  "WAYLAND_DISPLAY",
  "WSL_DISTRO_NAME",
  // Host executables the daemon runs on the user's behalf: `user_shell()`
  // wraps pane commands in `$SHELL` so rc files load, and the TUI opens
  // `$EDITOR`. Inherited on purpose, so a spec exercises the same programs
  // the developer has; `spawnAoeServe` prepends its shim dir to PATH.
  "EDITOR",
  "PATH",
  "SHELL",
  "VISUAL",
]);

const rust = join(web, "../src");
const files = rustFiles(rust);
const excluded = new Set();
for (const file of files) {
  const source = readFileSync(file, "utf8");
  const modules = /#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;/g;
  const base = ["lib.rs", "main.rs", "mod.rs"].includes(basename(file))
    ? dirname(file)
    : join(dirname(file), basename(file, ".rs"));
  for (const [, name] of source.matchAll(modules)) {
    excluded.add(join(base, name + ".rs"));
    excluded.add(join(base, name));
  }
}
const names = new Set();
const constants = new Map();
const constantReads = new Set();
const patterns = [
  /env::var(?:_os)?\(\s*"([A-Z][A-Z0-9_]*)"/g,
  /\benv\s*=\s*"([A-Z][A-Z0-9_]*)"/g,
  /"([A-Z][A-Z0-9_]*_(?:HOME|DIR|DB|PATH|CREDENTIALS))"/g,
];
for (const file of files) {
  if ([...excluded].some((path) => file === path || file.startsWith(path + "/"))) continue;
  const source = productionSource(readFileSync(file, "utf8"));
  for (const [, identifier, name] of source.matchAll(
    /const\s+([A-Z0-9_]+)\s*:\s*&(?:'static\s+)?str\s*=\s*"([A-Z][A-Z0-9_]+)"/g,
  )) {
    const values = constants.get(identifier) ?? new Set();
    values.add(name);
    constants.set(identifier, values);
  }
  for (const [, identifier] of source.matchAll(
    /env::var(?:_os)?\(\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Z][A-Z0-9_]*)\s*\)/g,
  ))
    constantReads.add(identifier);
  for (const pattern of patterns) {
    for (const [, name] of source.matchAll(pattern)) names.add(name);
  }
}
for (const identifier of constantReads) {
  for (const name of constants.get(identifier) ?? []) names.add(name);
}
if (names.size === 0) throw new Error("No production environment reads inventoried");
const host = "/host/dev";
const home = "/tmp/aoe-static-isolation";
const hostile = Object.fromEntries([...names].map((name) => [name, host + "/" + name.toLowerCase()]));
const env = isolateEnv(hostile, {
  home,
  xdgConfig: join(home, "config"),
  xdgData: join(home, "share"),
  tmp: join(home, "tmp"),
  tmuxTmp: join(home, "tmux"),
});
const unsafe = [...names].filter((name) => env[name]?.startsWith(host) && !INHERITED_BY_CONTRACT.has(name));
const missing = [...names].filter((name) => INHERITED_BY_CONTRACT.has(name) && !env[name]?.startsWith(host));
if (unsafe.length || missing.length) {
  throw new Error(
    "Environment isolation contract failed: unsafe host reads [" +
      unsafe.join(", ") +
      "], incorrectly dropped inherited reads [" +
      missing.join(", ") +
      "]",
  );
}

function rustFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? rustFiles(path) : entry.name.endsWith(".rs") ? [path] : [];
  });
}

// Mask literals/comments only for item boundaries; retain literal env names for inventory.
function productionSource(source) {
  const tokens = /r(#{0,})"[\s\S]*?"\1|"(?:\\[\s\S]|[^"\\])*"|'(?:\\.|[^'\\\n])'|\/\/[^\n]*|\/\*[\s\S]*?\*\//g;
  const spaces = (text) => text.replace(/[^\n]/g, " ");
  const masked = source.replace(tokens, spaces);
  const test = /#\[cfg\(test\)\]/g;
  const cuts = [];
  for (const match of masked.matchAll(test)) {
    let position = match.index + match[0].length;
    let parentheses = 0;
    let brackets = 0;
    let generics = 0;
    while (position < masked.length) {
      const char = masked[position];
      if (char === "(") parentheses++;
      else if (char === ")") parentheses--;
      else if (char === "[") brackets++;
      else if (char === "]") brackets--;
      else if (char === "<") generics++;
      else if (char === ">" && generics) generics--;
      if (!parentheses && !brackets && !generics && (char === "{" || char === ";" || char === ",")) break;
      position++;
    }
    if (masked[position] === "{") {
      let depth = 1;
      while (++position < masked.length && depth) {
        if (masked[position] === "{") depth++;
        else if (masked[position] === "}") depth--;
      }
    }
    cuts.push([match.index, position + 1]);
  }
  for (const [start, end] of cuts.reverse())
    source = source.slice(0, start) + spaces(source.slice(start, end)) + source.slice(end);
  return source.replace(tokens, (token) => (token.startsWith("//") || token.startsWith("/*") ? spaces(token) : token));
}
