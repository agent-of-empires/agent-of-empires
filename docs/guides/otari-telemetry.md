# Export Claude Code Usage to Otari

[Claude Code](https://code.claude.com/docs/en/monitoring-usage) can export usage events over OpenTelemetry (OTel). This guide sends those events from both host and Agent of Empires sandbox sessions to a self-hosted [Otari](https://github.com/mozilla-ai/otari) gateway.

This setup imports subscription-backed Claude Code usage for analytics. It does not route model requests through Otari or enforce an Otari budget. If a Claude Code session already routes requests through Otari with `ANTHROPIC_BASE_URL`, do not also export that session's telemetry to Otari. Doing both records the same traffic twice in cost analytics.

## Before you start

Create a dedicated, budget-exempt importer key in your standalone Otari deployment. Otari rejects budgeted keys for retrospective imports, and hybrid gateways do not serve the local OTLP ingest endpoints. Follow Otari's [Claude Code import guide](https://github.com/mozilla-ai/otari/blob/main/docs/use-with-claude-code.md#import-subscription-usage-without-routing-through-otari) for current key creation and deployment requirements.

Treat the importer key as a secret. Because it is budget-exempt, do not reuse it for live gateway traffic.

The examples use these placeholders:

- `https://otari.example.com`: the Otari root URL, without `/v1`
- `gw-your-exempt-key`: the dedicated importer key

Claude Code appends `/v1/logs` to the root URL. The `api_request` events on the logs signal contain the usage Otari imports. Traces are not required. The optional metrics exporter adds content-free outcome counters such as commits and lines changed, but it does not import usage by itself.

## Host Claude Code sessions

Add an `env` block to `~/.claude/settings.json`, merging it with any settings already present:

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

This file now contains the importer key. Keep it private and do not commit or share it. To include Otari's optional outcome counters, also set `"OTEL_METRICS_EXPORTER": "otlp"` in the same `env` object.

Restart Claude Code after changing the settings. New host sessions will export usage to Otari.

## AoE sandbox sessions

For sandbox sessions, keep the importer key out of the AoE configuration. Store the complete header in an environment variable on the host:

```sh
# ~/.zshenv
export AOE_OTARI_OTEL_HEADERS='Authorization=Bearer gw-your-exempt-key'
```

Use `~/.zshenv`, not `~/.zshrc`, when zsh launches AoE. `.zshrc` is only read by interactive shells and can be skipped by a non-interactive launch context. Open a new zsh after editing the file, or run `source ~/.zshenv`, before restarting AoE. If a service manager launches AoE, define the variable in that service's environment instead.

Add the OTel settings to the `sandbox.environment` list for the profile that launches the session. On macOS and Windows, profile overrides normally live at `~/.agent-of-empires/profiles/<profile>/config.toml`. On Linux, they live under `$XDG_CONFIG_HOME/agent-of-empires/profiles/<profile>/config.toml`, with `~/.config` as the default XDG config home. See the [configuration reference](configuration.md#file-locations) if your app directory differs.

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

The `KEY=$HOST_VAR` form makes AoE read the value from its host environment and inject it into the container as `KEY`. The secret therefore never appears in `config.toml`.

To include optional outcome counters, add `"OTEL_METRICS_EXPORTER=otlp"` to the same list.

### Profile precedence

AoE resolves global configuration first, then the active profile, then repository configuration. A profile's `sandbox.environment` list replaces the global list rather than extending it. If you use a named profile, edit that profile's `config.toml` and preserve its existing entries. Add the OTel entries separately to every profile that should export telemetry.

## Apply and verify

1. Make sure `AOE_OTARI_OTEL_HEADERS` is present in the environment that launches AoE.
2. If AoE was started with `aoe serve --daemon`, reload its configuration and environment:

   ```sh
   aoe serve --restart
   ```

   If the TUI creates your sessions, quit and relaunch it from the updated shell. A foreground, systemd, or launchd process must be restarted through the shell or service manager that launched it.
3. Start a fresh sandbox session. Existing containers keep the environment from the time they were created.
4. Find the container name in the AoE status bar. It follows the pattern `aoe-sandbox-<session-id-prefix>`.
5. Verify that each required variable is non-empty without printing the secret header:

   ```sh
   AOE_SANDBOX_NAME=aoe-sandbox-xxxxxxxx
   docker exec "$AOE_SANDBOX_NAME" sh -c '
     for name in \
       CLAUDE_CODE_ENABLE_TELEMETRY \
       OTEL_LOGS_EXPORTER \
       OTEL_EXPORTER_OTLP_PROTOCOL \
       OTEL_EXPORTER_OTLP_ENDPOINT \
       OTEL_EXPORTER_OTLP_HEADERS
     do
       if [ -n "$(printenv "$name")" ]; then
         printf "%s: set\n" "$name"
       else
         printf "%s: MISSING\n" "$name"
       fi
     done
   '
   ```

6. Run a prompt in the fresh Claude Code session. The logs exporter normally flushes within a few seconds.
7. Confirm that a `source = claude_code` row appears in Otari's Activity page for the importer key. If it does not, check the Otari gateway logs for requests to `/v1/logs`.

## Troubleshooting

- **The header is missing in the container:** confirm the variable is set in the exact shell or service environment that launched AoE, then restart AoE and create a new sandbox session.
- **Other sandbox variables disappeared:** restore the prior entries in the active profile's `sandbox.environment` list. The profile list replaces the global list.
- **Otari returns 403:** confirm the key is active, belongs to the intended user, and is budget-exempt.
- **Otari returns 404 for `/v1/logs`:** telemetry import requires a standalone Otari gateway, not a hybrid gateway.
- **A manual `curl` or Python probe is blocked while Claude Code works:** a reverse proxy or WAF can classify clients differently. Check its rules and Otari logs before changing the OTel configuration.
- **No row appears immediately:** wait for the logs export interval, which defaults to a few seconds, then generate another Claude Code request.

Historical backfills and Otari-side deployment setup are outside the scope of this guide. See Otari's [external usage documentation](https://github.com/mozilla-ai/otari/blob/main/docs/external-usage.md) for those workflows.
