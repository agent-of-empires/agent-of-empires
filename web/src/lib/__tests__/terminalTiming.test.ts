import { describe, it, expect } from "vitest";
import { TerminalTiming } from "../terminalTiming";

describe("TerminalTiming Idle-TTFB", () => {
  it("arms when idle and resolves on the next binary frame", () => {
    const t = new TerminalTiming();
    // lastBinaryRx starts at 0, so a keystroke at 200ms is idle.
    t.onKeystroke(200);
    const token = t.onBinaryFrame(250);
    expect(token).not.toBeNull();
    const snap = t.snapshot();
    expect(snap.ttfbSocketMs.count).toBe(1);
    expect(snap.ttfbSocketMs.p50).toBe(50);
  });

  it("does not arm when the terminal is busy (recent output)", () => {
    const t = new TerminalTiming();
    t.onBinaryFrame(100); // lastBinaryRx = 100
    t.onKeystroke(150); // 50ms gap <= 100ms idle threshold, not idle
    const token = t.onBinaryFrame(200);
    expect(token).toBeNull();
    expect(t.snapshot().ttfbSocketMs.count).toBe(0);
  });

  it("discards a sample whose echo arrives after the timeout", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    const token = t.onBinaryFrame(200 + 1001); // > 1000ms TTFB timeout
    expect(token).toBeNull();
    const snap = t.snapshot();
    expect(snap.ttfbSocketMs.count).toBe(0);
    expect(snap.discarded).toBe(1);
  });

  it("discards when a second keystroke arrives before the echo", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    t.onKeystroke(250); // overlapping, ambiguous
    expect(t.snapshot().outstandingKey).toBe(false);
    expect(t.snapshot().discarded).toBe(1);
    // The next frame resolves nothing.
    expect(t.onBinaryFrame(300)).toBeNull();
    expect(t.snapshot().ttfbSocketMs.count).toBe(0);
  });

  it("records render completion via the token", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    const token = t.onBinaryFrame(250);
    expect(token).not.toBeNull();
    t.onRender(token!, 260);
    expect(t.snapshot().ttfbRenderMs.p50).toBe(60);
  });

  it("prunes an armed sample that times out", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    t.pruneTimeouts(200 + 1001);
    const snap = t.snapshot();
    expect(snap.outstandingKey).toBe(false);
    expect(snap.discarded).toBe(1);
  });
});

describe("TerminalTiming WS control RTT", () => {
  it("computes round trip and server busy from a pong", () => {
    const t = new TerminalTiming();
    const ping = t.makePing(1000);
    expect(ping).not.toBeNull();
    t.onPong(ping!.seq, ping!.client_t, 2000, 1050);
    const snap = t.snapshot();
    expect(snap.wsControlRttMs.p50).toBe(50);
    expect(snap.serverBusyMs.p50).toBe(2); // 2000us -> 2ms
  });

  it("allows only one outstanding ping at a time", () => {
    const t = new TerminalTiming();
    expect(t.makePing(1000)).not.toBeNull();
    expect(t.makePing(1001)).toBeNull();
  });

  it("skips a ping while a keystroke sample is armed", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    expect(t.makePing(300)).toBeNull();
  });

  it("ignores a pong with a stale seq", () => {
    const t = new TerminalTiming();
    const ping = t.makePing(1000);
    t.onPong(ping!.seq + 99, 1000, 0, 1050);
    const snap = t.snapshot();
    expect(snap.wsControlRttMs.count).toBe(0);
    expect(snap.outstandingPing).toBe(true);
  });

  it("drops an outstanding ping that exceeds the timeout", () => {
    const t = new TerminalTiming();
    t.makePing(1000);
    t.pruneTimeouts(1000 + 2001);
    expect(t.snapshot().outstandingPing).toBe(false);
  });
});

describe("TerminalTiming percentiles and derived", () => {
  it("computes p50/p90/p95 over a known set", () => {
    const t = new TerminalTiming();
    for (let i = 1; i <= 10; i++) {
      const ping = t.makePing(0);
      t.onPong(ping!.seq, ping!.client_t, 0, i * 10);
    }
    const p = t.snapshot().wsControlRttMs;
    expect(p.count).toBe(10);
    expect(p.p50).toBe(50);
    expect(p.p90).toBe(90);
    expect(p.p95).toBe(100);
  });

  it("derives the stack delta as socket p50 minus ws-rtt p50", () => {
    const t = new TerminalTiming();
    // One socket sample of 80ms.
    t.onKeystroke(200);
    t.onBinaryFrame(280);
    // One ws rtt of 30ms.
    const ping = t.makePing(1000);
    t.onPong(ping!.seq, ping!.client_t, 0, 1030);
    expect(t.snapshot().derived.stackP50).toBe(50);
  });

  it("reports null stack delta until both metrics have samples", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    t.onBinaryFrame(280);
    expect(t.snapshot().derived.stackP50).toBeNull();
  });

  it("resets all accumulated state", () => {
    const t = new TerminalTiming();
    t.onKeystroke(200);
    t.onBinaryFrame(250);
    t.reset();
    const snap = t.snapshot();
    expect(snap.ttfbSocketMs.count).toBe(0);
    expect(snap.discarded).toBe(0);
  });
});
