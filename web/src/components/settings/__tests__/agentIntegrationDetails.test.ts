import { describe, expect, it } from "vitest";

import type { PluginAgentIntegration } from "../../../lib/api";
import { agentIntegrationDetails } from "../agentIntegrationDetails";

describe("agentIntegrationDetails", () => {
  it("discloses nothing beyond the command for a command-only integration", () => {
    expect(agentIntegrationDetails({ tool: "omp", command: "omp" })).toEqual([]);
  });

  it("names every launch, detection, and reply declaration shown in consent", () => {
    const integration: PluginAgentIntegration = {
      tool: "omp",
      command: "omp",
      launch: {
        oneshot_flag: "-p",
        instruction_flag: "--append-system-prompt {}",
        model_flag: "--model",
        yolo: { kind: "env", key: "OMP_YOLO", value: "1" },
        container_env: { OMP_HOME: "/state" },
        aliases: ["omp-agent"],
        resume: { kind: "flag_pair", existing: "--continue", new_session: "--new" },
        capture: { backend: "tmux", host: "pane", sandbox: "pane" },
        host_only: true,
        install_hint: "curl -fsSL https://omp.sh/install | sh",
      },
      detection: { mode: "screen" },
      reply_fallback: {
        allow: [
          { kind: "named", value: "yes" },
          { kind: "literal", value: "y" },
        ],
        deny: [{ kind: "named", value: "no" }],
      },
    };

    expect(agentIntegrationDetails(integration)).toEqual([
      "launch flags: oneshot -p; instruction --append-system-prompt {}; model --model; yolo OMP_YOLO=1",
      "sandbox environment: OMP_HOME=/state",
      "aliases: omp-agent",
      "resume: flags --continue / --new",
      "capture: tmux (host pane, sandbox pane)",
      "host only",
      "install: curl -fsSL https://omp.sh/install | sh",
      "detection: screen",
      'reply fallback: allow [yes, "y"], deny [no]',
    ]);
  });

  it("renders the flag, always, and subcommand variants", () => {
    expect(
      agentIntegrationDetails({
        tool: "tool",
        launch: {
          yolo: { kind: "flag", flag: "--yolo" },
          resume: { kind: "flag", flag: "--resume" },
        },
      }),
    ).toEqual(["launch flags: yolo --yolo", "resume: flag --resume"]);

    expect(
      agentIntegrationDetails({
        tool: "tool",
        launch: {
          yolo: { kind: "always" },
          resume: { kind: "subcommand", command: "resume" },
        },
      }),
    ).toEqual(["launch flags: yolo always", "resume: subcommand resume"]);
  });

  it("escapes control and bidi characters and names permanent approvals", () => {
    const details = agentIntegrationDetails({
      tool: "tool",
      launch: {
        container_env: { TOOL_HOME: "/tmp\u001b[2J" },
        install_hint: "install \u202enow",
      },
      reply_fallback: {
        allow: [{ kind: "named", value: "Enter" }],
        allow_always: [{ kind: "named", value: "Down" }],
        deny: [{ kind: "named", value: "Escape" }],
      },
    });
    expect(details).toEqual([
      "sandbox environment: TOOL_HOME=/tmp\\u{1b}[2J",
      "install: install \\u{202e}now",
      "reply fallback: allow [Enter], deny [Escape], allow always [Down]",
    ]);
  });
});
