# Export Claude Code Usage to Otari

[Claude Code](https://code.claude.com/docs/en/monitoring-usage) can export usage events over OpenTelemetry. This guide sends them, from host and AoE sandbox sessions alike, to a self-hosted [Otari](https://github.com/mozilla-ai/otari) gateway for analytics. It does not route model requests through Otari or enforce a budget, so if a session already routes requests through Otari with `ANTHROPIC_BASE_URL`, do not also export its telemetry: that records the same traffic twice.

## Before you start

Create a dedicated, budget-exempt importer key in a standalone Otari deployment. Otari rejects budgeted keys for retrospective imports, and hybrid gateways do not serve the local OTLP ingest endpoints; follow Otari's [Claude Code import guide](https://github.com/mozilla-ai/otari/blob/main/docs/use-with-claude-code.md#import-subscription-usage-without-routing-through-otari) for the current requirements. Treat the key as a secret and do not reuse it for live gateway traffic.

The examples use `https://otari.example.com` as the Otari root URL (without `/v1`) and `gw-your-exempt-key` as the importer key. Claude Code appends `/v1/logs`, and the `api_request` events on the logs signal carry the usage Otari imports. Traces are not required; the optional metrics exporter adds content-free outcome counters but imports no usage by itself.

"Content-free" is not anonymous: Claude Code telemetry can carry `user.email`, `user.account_uuid`, `user.account_id`, `organization.id`, the installation-scoped `user.id`, and `session.id`, so restrict access to the deployment accordingly. Prompt, response, and tool content are redacted by default; do not enable `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_TOOL_CONTENT`, or `OTEL_LOG_RAW_API_BODIES` unless you intend to export that content.

## Host sessions

Merge an `env` block into `~/.claude/settings.json`, then restart Claude Code:

```json
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "https://otari.example.com",
    "OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Bearer gw-your-exempt-key"
  }
}
```

That file now holds the importer key, so keep it private. Add `"OTEL_METRICS_EXPORTER": "otlp"` for the optional outcome counters.

## Sandbox sessions

Keep the key out of the AoE configuration by storing the whole header in the environment that launches AoE:

```sh
# ~/.zshenv, not ~/.zshrc: only .zshenv is read by non-interactive launches
export AOE_OTARI_OTEL_HEADERS='Authorization=Bearer gw-your-exempt-key'
```

Then add the OTel settings to the `sandbox.environment` list of the profile that launches the session (profiles live under the app directory; see the [configuration reference](configuration.md#file-locations)):

```toml
[sandbox]
environment = [
    # Keep any existing entries in this list.
    "CLAUDE_CODE_ENABLE_TELEMETRY=1",
    "OTEL_LOGS_EXPORTER=otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf",
    "OTEL_EXPORTER_OTLP_ENDPOINT=https://otari.example.com",
    "OTEL_EXPORTER_OTLP_HEADERS=$AOE_OTARI_OTEL_HEADERS",
]
```

The `KEY=$HOST_VAR` form makes AoE read the value from its own environment and inject it into the container, so the secret never appears in `config.toml`. A profile's `sandbox.environment` replaces the global list rather than extending it, so preserve the OTel entries in every applicable profile; a repo config cannot set this list at all.

## Apply and verify

1. Make sure `AOE_OTARI_OTEL_HEADERS` is set in the environment that launches AoE.
2. Reload AoE from that environment: `aoe serve --restart` for a daemon, or quit and relaunch the TUI. A foreground, systemd, or launchd process must be restarted through whatever launched it.
3. Start a **fresh** sandbox session; existing containers keep the environment they were created with.
4. Check the variables inside the container, without printing the header, using the container name from the AoE status bar (`aoe-sandbox-<session-id-prefix>`):

   ```sh
   docker exec aoe-sandbox-xxxxxxxx sh -c 'for n in \
     CLAUDE_CODE_ENABLE_TELEMETRY OTEL_LOGS_EXPORTER OTEL_EXPORTER_OTLP_PROTOCOL \
     OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_HEADERS; do
     [ -n "$(printenv "$n")" ] && echo "$n: set" || echo "$n: MISSING"; done'
   ```

5. Run a prompt in that session, then confirm a `source = claude_code` row appears on Otari's Activity page for the importer key. The logs exporter normally flushes within seconds; if nothing arrives, check the gateway logs for requests to `/v1/logs`.

## Troubleshooting

- **The header is missing in the container**: confirm the variable is set in the exact shell or service environment that launched AoE, restart AoE, and create a new sandbox session.
- **Other sandbox variables disappeared**: a profile list replaced the global one; restore the prior entries in the last list AoE applies.
- **Otari returns 403**: the key must be active, belong to the intended user, and be budget-exempt.
- **Otari returns 404 for `/v1/logs`**: telemetry import needs a standalone gateway, not a hybrid one.
- **A manual `curl` probe is blocked while Claude Code works**: a reverse proxy or WAF classifies clients differently; check its rules before changing the OTel configuration.

Historical backfills and Otari-side deployment are out of scope here; see Otari's [external usage documentation](https://github.com/mozilla-ai/otari/blob/main/docs/external-usage.md).
