import { describe, expect, it } from "vitest";
import { agentIntegrationDetails, displayAgentText } from "../agentIntegrationDetails";

describe("agentIntegrationDetails", () => {
  it("discloses launch, detection and fallback behavior safely", () => {
    expect(
      agentIntegrationDetails({
        tool: "agent",
        provider_versions: ["^1.0"],
        replies: false,
        launch: {
          aliases: ["alias"],
          oneshot_flag: "-p",
          instruction_flag: "--prompt={}",
          model_flag: "--model",
          yolo: { kind: "env", key: "AUTO", value: "1" },
          container_env: { HOME_DIR: "/tmp\u001b[2J" },
          install_hint: "install \u202enow",
          host_only: true,
          send_keys_enter_delay_ms: 150,
          resume: { kind: "flag_pair", existing: "--continue", new_session: "--new" },
          capture: { backend: "omp", host: "pane_scoped", sandbox: "unsupported" },
        },
        detection: { mode: "native_only" },
        reply_fallback: {
          allow: [{ kind: "literal", value: "y" }],
          deny: [{ kind: "named", value: "Escape" }],
          allow_always: [{ kind: "named", value: "Enter" }],
        },
      }),
    ).toEqual([
      "launch flags: oneshot -p; instruction --prompt={}; model --model; yolo AUTO=1",
      "enter delay: 150 ms",
      "sandbox environment: HOME_DIR=/tmp\\u{1b}[2J",
      "aliases: alias",
      "resume: flags --continue / --new",
      "capture: omp (host pane_scoped, sandbox unsupported)",
      "host only",
      "install: install \\u{202e}now",
      "detection: native_only",
      "provider versions: ^1.0",
      "native replies: no",
      'reply fallback: allow ["y"], deny [Escape], allow always [Enter]',
    ]);
    expect(displayAgentText("a\u202eb\u001b")).toBe("a\\u{202e}b\\u{1b}");
  });

  it("discloses alternative launch variants", () => {
    expect(
      agentIntegrationDetails({
        tool: "agent",
        launch: { yolo: { kind: "flag", flag: "--yes" }, resume: { kind: "flag", flag: "--resume" } },
      }),
    ).toEqual(["launch flags: yolo --yes", "resume: flag --resume", "native replies: no"]);
    expect(
      agentIntegrationDetails({
        tool: "agent",
        replies: true,
        launch: { yolo: { kind: "always" }, resume: { kind: "subcommand", command: "resume" } },
      }),
    ).toEqual(["launch flags: yolo always", "resume: subcommand resume", "native replies: yes"]);
  });
  it("discloses the ACP declaration with host-only keys marked and text escaped", () => {
    expect(
      agentIntegrationDetails({
        tool: "agent",
        acp: {
          id: "agent-acp",
          display_name: "Agent\u202e",
          proxy: { command: "bin/proxy" },
          adapter: { upstream_package: "@acme/agent-acp", upstream_pin: "2.1.0" },
          env: { allowlist: ["KEY", "HOME_DIR"], host_only: ["HOME_DIR"], defaults: { REGION: "eu" } },
          compat: {
            expected_agent_name: "@acme/agent-acp",
            min_version: "2.0.0",
            capability_min_versions: { steering: "2.1.0" },
          },
          profile: { modes: [{ id: "auto", approval_class: "unattended" }] },
          failure_policy: "refuse",
        },
      }),
    ).toEqual([
      "native replies: no",
      "Provides ACP agent agent-acp (Agent\\u{202e}) through proxy bin/proxy",
      "ACP adapter: @acme/agent-acp@2.1.0",
      "ACP env forwarded: KEY, HOME_DIR (host only)",
      "ACP env defaults: REGION=eu",
      "ACP compat: name @acme/agent-acp; min version 2.0.0; steering from 2.1.0",
      "ACP modes: auto (unattended)",
      "ACP failure policy: refuse the session",
    ]);
  });

  it("omits absent ACP sections and an empty launch table", () => {
    expect(
      agentIntegrationDetails({
        tool: "agent",
        launch: { container_env: {} },
        acp: {
          id: "min",
          aliases: ["min-cli", "m"],
          display_name: "Min",
          proxy: { command: "bin/p" },
          compat: { capability_min_versions: { steering: "1.2.0" } },
          failure_policy: "neutral_boot",
        },
      }),
    ).toEqual([
      "native replies: no",
      "Provides ACP agent min (Min) through proxy bin/p",
      "ACP aliases: min-cli, m",
      "ACP compat: steering from 1.2.0",
      "ACP failure policy: start on the neutral boot",
    ]);
  });
});
