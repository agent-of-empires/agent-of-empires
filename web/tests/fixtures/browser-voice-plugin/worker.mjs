#!/usr/bin/env node

import { writeFileSync } from "node:fs";
import { createInterface } from "node:readline";

let nextId = 1;
let sessionId = null;
const pending = new Map();

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function request(method, params) {
  const id = nextId++;
  send({ jsonrpc: "2.0", id, method, params });
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

async function publishAction(payload) {
  await request("ui.state.set", {
    slot: "composer-action",
    id: "dictate",
    session_id: sessionId,
    payload,
  });
}

function fail(message) {
  writeFileSync("voice-received.json", JSON.stringify({ ok: false, error: message }));
  process.stderr.write(`${message}\n`);
}

async function handleVoice(params) {
  const browser = params?.browser;
  const audio = browser?.audio;
  if (params?.session_id !== sessionId) throw new Error("voice notification used the wrong session");
  if (Object.hasOwn(params ?? {}, "composer")) throw new Error("voice notification leaked the composer snapshot");
  if (browser?.action !== "voice-input") throw new Error("voice notification omitted browser.action");
  if (typeof browser?.capture_id !== "string" || browser.capture_id.length === 0) {
    throw new Error("voice notification omitted capture_id");
  }
  if (typeof audio?.mime_type !== "string" || !audio.mime_type.startsWith("audio/")) {
    throw new Error("voice notification omitted audio MIME type");
  }
  if (!Number.isInteger(audio?.bytes) || audio.bytes <= 0) throw new Error("voice notification audio was empty");
  if (!Number.isInteger(audio?.duration_ms) || audio.duration_ms <= 0) {
    throw new Error("voice notification omitted duration_ms");
  }
  if (typeof audio?.data_base64 !== "string" || audio.data_base64.length === 0) {
    throw new Error("voice notification omitted base64 audio");
  }

  writeFileSync(
    "voice-received.json",
    JSON.stringify({
      ok: true,
      capture_id: browser.capture_id,
      mime_type: audio.mime_type,
      bytes: audio.bytes,
      duration_ms: audio.duration_ms,
      leaked_composer: false,
    }),
  );
  await publishAction({
    label: "Dictate",
    method: "voice.transcribe",
    icon: "mic",
    tooltip: "Record dictation",
    browser_action: { kind: "voice-input" },
    draft_operation: {
      kind: "replace-selection",
      id: `transcript-${browser.capture_id}`,
      text: "dictated by the live worker",
      capture_id: browser.capture_id,
    },
  });
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id !== undefined && (message.result !== undefined || message.error !== undefined)) {
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(message.error.message ?? "host RPC failed"));
    else waiter.resolve(message.result);
    return;
  }
  if (message.method === "voice.transcribe") {
    void handleVoice(message.params).catch((error) => fail(error instanceof Error ? error.message : String(error)));
  }
});

try {
  const listed = await request("sessions.list", {});
  sessionId = listed?.sessions?.[0]?.id ?? null;
  if (!sessionId) throw new Error("fixture could not discover a seeded session");
  await publishAction({
    label: "Dictate",
    method: "voice.transcribe",
    icon: "mic",
    tooltip: "Record dictation",
    browser_action: { kind: "voice-input" },
  });
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
