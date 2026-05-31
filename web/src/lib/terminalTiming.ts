/**
 * Keystroke-to-echo latency instrumentation for the web terminal, active
 * only under `?debug=terminal-timing`. Pure logic with no DOM or React so
 * it can be unit tested in isolation. See #1453.
 *
 * Two complementary measurements:
 *
 *  - Idle-TTFB (time to first byte): the experiential metric. When the
 *    user presses a key after the terminal has been quiet for at least
 *    IDLE_GAP_MS, the send is stamped and resolved on the very next
 *    inbound binary (PTY) frame. There is deliberately no byte-level echo
 *    matching: shells, TUIs, autosuggestions and password prompts make
 *    the echoed bytes unrelated to what was typed, so matching bytes
 *    produces garbage percentiles. Gating on idle and resolving on the
 *    first frame measures the real key-to-screen path without parsing the
 *    echo. A render callback (fed via `onRender`) captures the extra time
 *    xterm spends painting that frame, which is what separates a slow
 *    WebGL/DOM renderer from a slow network.
 *
 *  - WS control RTT: the attribution metric. A timing_ping/timing_pong
 *    round trip on the control channel that never touches the PTY. The
 *    server reports its own recv-to-send duration (`server_busy_us`), so
 *    `client_rtt - server_busy` approximates network plus WebSocket
 *    transit with no client/server clock synchronisation. The gap between
 *    Idle-TTFB and WS control RTT is the server plus PTY plus tmux plus
 *    agent contribution.
 */

/** Terminal must be quiet this long before a keystroke is sampled, so
 *  background output (a running `tail -f`) is not mistaken for an echo. */
const IDLE_GAP_MS = 100;
/** An armed keystroke with no inbound frame within this window is treated
 *  as never echoed (a non-echoing key) and discarded rather than resolved
 *  against unrelated later output. */
const TTFB_TIMEOUT_MS = 1000;
/** An outstanding ping with no pong within this window is dropped so the
 *  next tick can probe again. */
const PING_TIMEOUT_MS = 2000;
/** Cap on retained raw samples per category. Plenty for p95 over a
 *  multi-minute debug session while bounding memory. */
const MAX_SAMPLES = 5000;

export type TerminalRenderer = "webgl" | "dom" | "unknown";

export interface Percentiles {
  count: number;
  p50: number;
  p90: number;
  p95: number;
}

export interface TerminalTimingSnapshot {
  renderer: TerminalRenderer;
  /** Key send to first inbound binary frame. */
  ttfbSocketMs: Percentiles;
  /** Key send to xterm render callback for the resolving frame. */
  ttfbRenderMs: Percentiles;
  /** timing_ping to timing_pong, full client-observed round trip. */
  wsControlRttMs: Percentiles;
  /** Server-reported recv-to-send duration of the pong. */
  serverBusyMs: Percentiles;
  derived: {
    /** ttfbSocket p50 minus wsControlRtt p50: rough server + PTY + stack
     *  share of the key-to-screen latency. Null until both have samples. */
    stackP50: number | null;
  };
  outstandingKey: boolean;
  outstandingPing: boolean;
  discarded: number;
}

export interface TerminalTimingDump extends TerminalTimingSnapshot {
  raw: {
    ttfbSocketMs: number[];
    ttfbRenderMs: number[];
    wsControlRttMs: number[];
    serverBusyMs: number[];
  };
  connection: { effectiveType?: string; rtt?: number; downlink?: number } | null;
}

/** Token returned by `onBinaryFrame` when that frame resolved an armed
 *  keystroke. Pass it to `onRender` from the xterm write callback. */
export interface RenderToken {
  tSend: number;
}

/** Outbound ping descriptor. The client sends `{type:"timing_ping",seq,
 *  client_t}` and echoes `client_t` back through the pong unchanged. */
export interface PingFrame {
  seq: number;
  client_t: number;
}

function percentile(sorted: number[], q: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil(q * sorted.length) - 1),
  );
  return sorted[idx]!;
}

function round1(n: number): number {
  return Math.round(n * 10) / 10;
}

function summarize(samples: number[]): Percentiles {
  const sorted = [...samples].sort((a, b) => a - b);
  return {
    count: sorted.length,
    p50: round1(percentile(sorted, 0.5)),
    p90: round1(percentile(sorted, 0.9)),
    p95: round1(percentile(sorted, 0.95)),
  };
}

export class TerminalTiming {
  private renderer: TerminalRenderer = "unknown";
  private lastBinaryRx = 0;
  private armed: { tSend: number } | null = null;
  private pendingPing: PingFrame | null = null;
  private pingSeq = 0;
  private discarded = 0;

  private ttfbSocket: number[] = [];
  private ttfbRender: number[] = [];
  private wsRtt: number[] = [];
  private serverBusy: number[] = [];

  setRenderer(renderer: TerminalRenderer): void {
    this.renderer = renderer;
  }

  /** Call on every outbound keystroke (`term.onData`). Arms a sample only
   *  when the terminal is idle and nothing is already in flight. */
  onKeystroke(now: number): void {
    if (this.armed) {
      // A second key before the first echo returned: the clean
      // single-outstanding measurement is gone. Drop it and stay disarmed
      // (the terminal is now busy with overlapping echoes).
      this.armed = null;
      this.discarded += 1;
      return;
    }
    if (now - this.lastBinaryRx <= IDLE_GAP_MS) return;
    this.armed = { tSend: now };
  }

  /** Call on every inbound binary (PTY) frame, before writing it to xterm.
   *  Returns a token to feed `onRender` when this frame resolved an armed
   *  keystroke, otherwise null. */
  onBinaryFrame(now: number): RenderToken | null {
    const armed = this.armed;
    this.lastBinaryRx = now;
    if (!armed) return null;
    this.armed = null;
    const elapsed = now - armed.tSend;
    if (elapsed > TTFB_TIMEOUT_MS) {
      // Too late to be this key's echo (the key likely did not echo);
      // discard rather than record a bogus multi-second TTFB.
      this.discarded += 1;
      return null;
    }
    push(this.ttfbSocket, elapsed);
    return { tSend: armed.tSend };
  }

  /** Call from the xterm write callback for a frame that `onBinaryFrame`
   *  flagged, to record the time through render completion. */
  onRender(token: RenderToken, now: number): void {
    push(this.ttfbRender, now - token.tSend);
  }

  /** Build the next ping, or null if one is already outstanding or a
   *  keystroke sample is armed (skipped to keep the experiential metric
   *  free of self-induced head-of-line contention). */
  makePing(now: number): PingFrame | null {
    if (this.pendingPing || this.armed) return null;
    const ping: PingFrame = { seq: this.pingSeq++, client_t: now };
    this.pendingPing = ping;
    return ping;
  }

  /** Resolve a pong. `clientT` is the value echoed back unchanged;
   *  `serverBusyUs` is the server's own recv-to-send duration. */
  onPong(seq: number, clientT: number, serverBusyUs: number, now: number): void {
    if (!this.pendingPing || this.pendingPing.seq !== seq) return;
    this.pendingPing = null;
    push(this.wsRtt, now - clientT);
    push(this.serverBusy, serverBusyUs / 1000);
  }

  /** Discard an armed keystroke or outstanding ping that has exceeded its
   *  timeout. Call periodically (the ping and summary intervals do). */
  pruneTimeouts(now: number): void {
    if (this.armed && now - this.armed.tSend > TTFB_TIMEOUT_MS) {
      this.armed = null;
      this.discarded += 1;
    }
    if (this.pendingPing && now - this.pendingPing.client_t > PING_TIMEOUT_MS) {
      this.pendingPing = null;
      this.discarded += 1;
    }
  }

  snapshot(): TerminalTimingSnapshot {
    const ttfbSocketMs = summarize(this.ttfbSocket);
    const wsControlRttMs = summarize(this.wsRtt);
    const stackP50 =
      ttfbSocketMs.count > 0 && wsControlRttMs.count > 0
        ? round1(ttfbSocketMs.p50 - wsControlRttMs.p50)
        : null;
    return {
      renderer: this.renderer,
      ttfbSocketMs,
      ttfbRenderMs: summarize(this.ttfbRender),
      wsControlRttMs,
      serverBusyMs: summarize(this.serverBusy),
      derived: { stackP50 },
      outstandingKey: this.armed !== null,
      outstandingPing: this.pendingPing !== null,
      discarded: this.discarded,
    };
  }

  dump(): TerminalTimingDump {
    return {
      ...this.snapshot(),
      raw: {
        ttfbSocketMs: [...this.ttfbSocket],
        ttfbRenderMs: [...this.ttfbRender],
        wsControlRttMs: [...this.wsRtt],
        serverBusyMs: [...this.serverBusy],
      },
      connection: readConnection(),
    };
  }

  summaryLine(): string {
    const s = this.snapshot();
    const stack = s.derived.stackP50 === null ? "n/a" : `${s.derived.stackP50}ms`;
    return (
      `[terminal.timing] renderer=${s.renderer} ` +
      `key-socket p50/p95=${s.ttfbSocketMs.p50}/${s.ttfbSocketMs.p95}ms ` +
      `key-render p50/p95=${s.ttfbRenderMs.p50}/${s.ttfbRenderMs.p95}ms ` +
      `ws-rtt p50/p95=${s.wsControlRttMs.p50}/${s.wsControlRttMs.p95}ms ` +
      `server-busy p50=${s.serverBusyMs.p50}ms ` +
      `stack(socket-rtt) p50=${stack} ` +
      `samples=${s.ttfbSocketMs.count} discarded=${s.discarded}`
    );
  }

  reset(): void {
    this.lastBinaryRx = 0;
    this.armed = null;
    this.pendingPing = null;
    this.discarded = 0;
    this.ttfbSocket = [];
    this.ttfbRender = [];
    this.wsRtt = [];
    this.serverBusy = [];
  }
}

function push(arr: number[], value: number): void {
  arr.push(value);
  if (arr.length > MAX_SAMPLES) arr.shift();
}

function readConnection(): TerminalTimingDump["connection"] {
  if (typeof navigator === "undefined") return null;
  const conn = (
    navigator as Navigator & {
      connection?: { effectiveType?: string; rtt?: number; downlink?: number };
    }
  ).connection;
  if (!conn) return null;
  return {
    effectiveType: conn.effectiveType,
    rtt: conn.rtt,
    downlink: conn.downlink,
  };
}
