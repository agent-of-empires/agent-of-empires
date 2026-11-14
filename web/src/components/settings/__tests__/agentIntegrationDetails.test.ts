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
});
