import type { PluginAgentIntegration } from "../../lib/api";

/** One line per non-command declaration so consent names every behavior change. */
export function agentIntegrationDetails(agent: PluginAgentIntegration): string[] {
  const details: string[] = [];
  const launch = agent.launch;
  if (launch) {
    const flags: string[] = [];
    if (launch.oneshot_flag) flags.push(`oneshot ${launch.oneshot_flag}`);
    if (launch.instruction_flag) flags.push(`instruction ${launch.instruction_flag}`);
    if (launch.model_flag) flags.push(`model ${launch.model_flag}`);
    if (launch.yolo) {
      flags.push(
        launch.yolo.kind === "flag"
          ? `yolo ${launch.yolo.flag}`
          : launch.yolo.kind === "env"
            ? `yolo ${launch.yolo.key}=${launch.yolo.value}`
            : "yolo always",
      );
    }
    if (flags.length > 0) details.push(`launch flags: ${flags.join("; ")}`);

    const env = Object.entries(launch.container_env ?? {});
    if (env.length > 0) details.push(`sandbox environment: ${env.map(([key, value]) => `${key}=${value}`).join(", ")}`);
    if (launch.aliases?.length) details.push(`aliases: ${launch.aliases.join(", ")}`);
    if (launch.resume) {
      const resume =
        launch.resume.kind === "flag"
          ? `flag ${launch.resume.flag}`
          : launch.resume.kind === "subcommand"
            ? `subcommand ${launch.resume.command}`
            : `flags ${launch.resume.existing} / ${launch.resume.new_session}`;
      details.push(`resume: ${resume}`);
    }
    if (launch.capture) {
      details.push(
        `capture: ${launch.capture.backend} (host ${launch.capture.host}, sandbox ${launch.capture.sandbox})`,
      );
    }
    if (launch.host_only) details.push("host only");
    if (launch.install_hint) details.push(`install: ${launch.install_hint}`);
  }
  if (agent.detection) details.push(`detection: ${agent.detection.mode}`);
  if (agent.reply_fallback) {
    const label = (key: { kind: string; value: string }) =>
      key.kind === "named" ? key.value : JSON.stringify(key.value);
    details.push(
      `reply fallback: allow [${agent.reply_fallback.allow.map(label).join(", ")}], deny [${agent.reply_fallback.deny
        .map(label)
        .join(", ")}]`,
    );
  }
  return details;
}
