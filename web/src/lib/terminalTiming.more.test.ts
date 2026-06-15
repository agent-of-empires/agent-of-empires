import { afterEach, describe, expect, it, vi } from "vitest";

import { TerminalTiming } from "./terminalTiming";

// Covers the reporting surface (setRenderer, snapshot/derived edge,
// summaryLine, dump + readConnection) and the MAX_SAMPLES ring-buffer
// shift, which the canonical __tests__/terminalTiming.test.ts never
// exercises.

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("TerminalTiming reporting surface", () => {
  it("threads the renderer through the snapshot", () => {
    const t = new TerminalTiming();
    expect(t.snapshot().renderer).toBe("unknown");
    t.setRenderer("webgl");
    expect(t.snapshot().renderer).toBe("webgl");
    t.setRenderer("dom");
    expect(t.snapshot().renderer).toBe("dom");
  });

  it("summaryLine reports n/a stack and zeroed metrics when empty", () => {
    const t = new TerminalTiming();
    const line = t.summaryLine();
    expect(line).toContain("renderer=unknown");
    expect(line).toContain("stack(socket-rtt) p50=n/a");
    expect(line).toContain("samples=0 discarded=0");
  });

  it("summaryLine renders a concrete stack delta once both metrics have samples", () => {
    const t = new TerminalTiming();
    // One socket sample of 80ms.
    t.onKeystroke(200);
    t.onBinaryFrame(280);
    // One ws rtt of 30ms -> stack delta 50ms.
    const ping = t.makePing(1000);
    t.onPong(ping!.seq, ping!.client_t, 4000, 1030);
    const line = t.summaryLine();
    expect(line).toContain("stack(socket-rtt) p50=50ms");
    expect(line).toContain("key-socket p50/p95=80/80ms");
    expect(line).toContain("ws-rtt p50/p95=30/30ms");
    // 4000us server busy -> 4ms.
    expect(line).toContain("server-busy p50=4ms");
    expect(line).toContain("samples=1");
  });
});

describe("TerminalTiming.dump connection probing", () => {
  it("returns null connection when navigator is undefined", () => {
    vi.stubGlobal("navigator", undefined);
    const t = new TerminalTiming();
    t.onKeystroke(200);
    t.onBinaryFrame(250);
    const d = t.dump();
    expect(d.connection).toBeNull();
    // Raw arrays are mirrored into the dump.
    expect(d.raw.ttfbSocketMs).toEqual([50]);
  });

  it("returns null connection when navigator has no connection field", () => {
    vi.stubGlobal("navigator", {});
    const t = new TerminalTiming();
    expect(t.dump().connection).toBeNull();
  });

  it("reads effectiveType/rtt/downlink from navigator.connection", () => {
    vi.stubGlobal("navigator", {
      connection: { effectiveType: "4g", rtt: 50, downlink: 10 },
    });
    const t = new TerminalTiming();
    const ping = t.makePing(1000);
    t.onPong(ping!.seq, ping!.client_t, 2000, 1050);
    const d = t.dump();
    expect(d.connection).toEqual({
      effectiveType: "4g",
      rtt: 50,
      downlink: 10,
    });
    expect(d.raw.wsControlRttMs).toEqual([50]);
    expect(d.raw.serverBusyMs).toEqual([2]);
  });

  it("dump carries the ttfbRender raw samples", () => {
    vi.stubGlobal("navigator", {});
    const t = new TerminalTiming();
    t.onKeystroke(200);
    const token = t.onBinaryFrame(250);
    t.onRender(token!, 260);
    expect(t.dump().raw.ttfbRenderMs).toEqual([60]);
  });
});

describe("TerminalTiming MAX_SAMPLES ring buffer", () => {
  it("caps retained socket samples at 5000, dropping the oldest", () => {
    const t = new TerminalTiming();
    // Each iteration: a fresh idle keystroke + immediate echo records one
    // socket sample. Bump the clock well past IDLE_GAP_MS each round so
    // the keystroke always arms.
    let clock = 0;
    for (let i = 0; i < 5001; i++) {
      clock += 1000;
      t.onKeystroke(clock);
      t.onBinaryFrame(clock + 1); // 1ms TTFB sample
    }
    const snap = t.snapshot();
    // Count is capped at MAX_SAMPLES even though 5001 were pushed.
    expect(snap.ttfbSocketMs.count).toBe(5000);
    // Every retained sample is the constant 1ms TTFB.
    expect(snap.ttfbSocketMs.p50).toBe(1);
    expect(snap.ttfbSocketMs.p95).toBe(1);
  });
});
