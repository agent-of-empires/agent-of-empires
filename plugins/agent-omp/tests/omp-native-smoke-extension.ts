import { randomUUID } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent/extensibility/extensions";

function delay(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

type Report = {
  instance_id: string; provider_session_id: string; writer_pid: number; revision: number;
  auto_approve?: { enabled: boolean; reversible: boolean };
  control_ack?: { request_id: string; enabled: boolean; error: string | null };
};

export default function (pi: ExtensionAPI): void {
  pi.on("session_start", async (_event, ctx) => {
    const marker = process.env.OMP_SMOKE_MARKER;
    const instanceId = process.env.AOE_INSTANCE_ID;
    try {
      if (!marker || !instanceId || typeof process.getuid !== "function") throw new Error("missing smoke identity");
      const reportPath = `/tmp/aoe-hooks-${process.getuid()}/${instanceId}/integration-omp.agent-of-empires.json`;
      const controlPath = reportPath.replace(/\.json$/, ".control.json");
      // Verify the compiled host's optional module loader.
      const { lookup } = await import("@oh-my-pi/pi-coding-agent/config/registry");
      const mode = lookup("tools.approvalMode");
      let report: Report | undefined;
      for (let attempt = 0; attempt < 40; attempt++) {
        try {
          const candidate = JSON.parse(readFileSync(reportPath, "utf8")) as Report;
          if (candidate.instance_id === instanceId && candidate.writer_pid === process.pid && candidate.auto_approve) {
            report = candidate;
            break;
          }
        } catch { /* Wait for the provider's first atomic publication. */ }
        await delay(50);
      }
      if (!report) throw new Error("provider did not advertise native policy control");
      const forced = process.env.OMP_SMOKE_FORCED === "1";
      if (report.auto_approve?.reversible === forced) throw new Error("incorrect force-on capability");
      const expectedId = process.env.OMP_SMOKE_EXPECTED_ID;
      if (expectedId && (report.provider_session_id !== expectedId || ctx.sessionManager.getSessionId() !== expectedId ||
          !ctx.sessionManager.getBranch().some(entry => entry.type === "custom" && entry.customType === "aoe-approval-smoke") ||
          report.auto_approve?.enabled || mode.get(pi.pi.settings) !== "always-ask")) {
        throw new Error("resumed native process did not retain conversation entries and safe launch policy");
      }
      for (const enabled of forced ? [false] : [true, false]) {
        const requestId = randomUUID();
        writeFileSync(controlPath, JSON.stringify({ schema_version: 1, instance_id: instanceId,
          provider_session_id: report.provider_session_id, writer_pid: process.pid, revision: report.revision,
          request_id: requestId, enabled }), { mode: 0o600 });
        let acknowledged = false;
        for (let attempt = 0; attempt < 80; attempt++) {
          await delay(25);
          report = JSON.parse(readFileSync(reportPath, "utf8")) as Report;
          if (report.control_ack?.request_id !== requestId) continue;
          if (forced) {
            if (!report.control_ack.error || !report.control_ack.enabled) throw new Error("force-on OFF was not rejected");
          } else if (report.control_ack.error || report.control_ack.enabled !== enabled ||
              mode.get(pi.pi.settings) !== (enabled ? "yolo" : "always-ask")) {
            throw new Error("native registry policy did not match acknowledgment");
          }
          acknowledged = true;
          break;
        }
        if (!acknowledged) throw new Error("native control acknowledgment timed out");
      }
      if (!expectedId) {
        const resumeFile = process.env.OMP_SMOKE_RESUME_FILE;
        if (!resumeFile) throw new Error("missing resume fixture path");
        pi.appendEntry("aoe-approval-smoke", { retained: true });
        // OMP persists sessions lazily; export the native session without a provider response.
        writeFileSync(resumeFile, [ctx.sessionManager.getHeader(), ...ctx.sessionManager.getEntries()]
          .map(entry => JSON.stringify(entry)).join("\n") + "\n", { mode: 0o600 });
        writeFileSync(`${marker}.sid`, report.provider_session_id, { mode: 0o600 });
      }
      writeFileSync(marker, "integration-snapshot-ok\n", { mode: 0o600 });
    } catch (error) {
      if (marker) writeFileSync(marker, `integration-failed: ${String(error)}\n`, { mode: 0o600 });
    } finally {
      ctx.shutdown();
    }
  });
}
