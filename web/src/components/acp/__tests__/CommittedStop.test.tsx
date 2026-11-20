// @vitest-environment jsdom

import { Profiler } from "react";
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { clearAcpCache } from "../../../hooks/useAcpSession";
import {
  currentControlSnapshot,
  flushAsync,
  installAcpFakes,
  json,
  lastSocket,
  profileWrapper,
  type Call,
} from "../../../hooks/__tests__/acpHarness";
import { nativeInputAvailable } from "../../../lib/acpTypes";
import { AcpRuntime, type AcpContext } from "../AcpRuntime";
import { Composer } from "../Composer";

let calls: Call[];

beforeEach(() => {
  localStorage.clear();
  clearAcpCache();
  calls = installAcpFakes(({ method, url }) => {
    if (method === "POST" && url.endsWith("/acp/prompt")) return json({ disposition: "sent" }, 202);
    if (method === "POST" && url.endsWith("/acp/cancel")) return json({}, 202);
    if (url === "/api/agents") return json([]);
    return undefined;
  });
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent() {
      return false;
    },
  }));
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  clearAcpCache();
  localStorage.clear();
});

it("the committed Composer Stop cancels its current turn before SDK passive publication, never a stale turn", async () => {
  const id = "committed-stop";
  const echoSeq = 1;
  let current!: AcpContext;
  let armed = false;
  let gestures = 0;
  let observed: { turn: number; ready: boolean; open: boolean; enabled: boolean } | undefined;
  const posts = (path: string) => calls.filter((call) => call.method === "POST" && call.url.endsWith(path));
  render(
    <Profiler
      id="committed-stop"
      onRender={() => {
        const button = document.querySelector<HTMLButtonElement>('button[aria-label="Stop"]');
        if (!armed || current.state.lastSeq !== echoSeq || !button || button.disabled) return;
        armed = false;
        gestures += 1;
        observed = {
          turn: current.state.promptSeq,
          ready: nativeInputAvailable(current.state),
          open: current.status === "open",
          enabled: !button.disabled,
        };
        button.click();
      }}
    >
      <AcpRuntime sessionId={id}>
        {(ctx) => {
          current = ctx;
          return (
            <Composer
              sessionId={id}
              currentAgent={ctx.state.agent}
              currentProvider={null}
              availableModes={ctx.state.availableModes}
              currentModeId={ctx.state.currentModeId}
              legacyMode={ctx.state.mode}
              configOptions={ctx.state.configOptions}
              pendingConfigOption={ctx.state.pendingConfigOption}
              setConfigOption={ctx.setConfigOption}
              sessionUsage={ctx.state.sessionUsage}
              authStatus={ctx.state.authStatus}
              availableCommands={ctx.state.availableCommands}
              connected={ctx.status === "open" && nativeInputAvailable(ctx.state)}
              canForceEndTurn={ctx.canForceEndTurn}
              stopTurn={ctx.stopTurn}
              canSubmit={ctx.canSendQueuedNow}
              turnActive={ctx.state.turnActive}
              enqueuePrompt={ctx.sendPrompt}
              promptCapabilities={ctx.state.promptCapabilities}
              pendingAttachments={ctx.pendingAttachments}
              setPendingAttachments={ctx.setPendingAttachments}
              queuedPrompts={ctx.state.queuedPrompts}
              editQueuedPrompt={ctx.editQueuedPrompt}
            />
          );
        }}
      </AcpRuntime>
    </Profiler>,
    { wrapper: profileWrapper },
  );
  await flushAsync();
  lastSocket().open();
  await flushAsync();
  lastSocket().message(currentControlSnapshot(current.state, { ready: true }));
  await flushAsync();
  await act(() => current.sendPrompt("begin the retained turn"));
  await flushAsync();
  expect(posts("/acp/prompt")).toHaveLength(1);
  expect(current.state.turnActive).toBe(true);
  const previousTurn = current.state.promptSeq;
  const previousStop = current.stopTurn;
  armed = true;
  lastSocket().message({
    session_id: id,
    seq: echoSeq,
    event: { UserPromptSent: { text: "begin the retained turn" } },
  });
  await flushAsync();
  expect(current.state.promptSeq).toBeGreaterThan(previousTurn);
  expect(gestures).toBe(1);
  expect(observed).toEqual({ turn: current.state.promptSeq, ready: true, open: true, enabled: true });
  expect(posts("/acp/cancel")).toHaveLength(1);
  expect(current.canForceEndTurn).toBe(true);
  expect(current.state.interactionError).toBeNull();
  await act(async () => {
    previousStop();
  });
  await flushAsync();
  expect(posts("/acp/cancel")).toHaveLength(1);
  expect(posts("/acp/force_end_turn")).toEqual([]);
});
