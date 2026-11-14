import type { ReactNode } from "react";
import { usePushSubscription } from "../../../hooks/usePushSubscription";
import type { ServerAbout, SetupStatus } from "../../../lib/api";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import { SchemaSection } from "../../settings/SchemaSection";
import { GhostButton, PANEL, Section } from "../ui";

interface Props {
  status: SetupStatus;
  about: ServerAbout | null;
  schema: SettingsFieldDescriptor[];
  values: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => Promise<boolean>;
}

function PushRow() {
  const { state, enable } = usePushSubscription();
  let detail: ReactNode = "Not allowed yet. Your browser will ask once.";
  let action: ReactNode = <GhostButton onClick={() => void enable()}>Allow notifications</GhostButton>;
  if (state.kind === "enabled" || state.kind === "sending-test") {
    detail = "Allowed on this browser.";
    action = null;
  } else if (state.kind === "denied") {
    detail = "Blocked in this browser's site settings.";
    action = null;
  } else if (state.kind === "unsupported") {
    detail =
      state.reason === "insecure-origin"
        ? "Unavailable over plain HTTP. Browsers only allow push on HTTPS or localhost."
        : state.reason === "ios-not-standalone"
          ? "Add this dashboard to your Home Screen first; iOS only allows push for installed apps."
          : "This browser does not support push.";
    action = null;
  } else if (state.kind === "disabled-by-server") {
    detail = "Turned off on the Daemon.";
    action = null;
  }
  return (
    <div className="px-4 py-3 flex items-center justify-between gap-3">
      <div>
        <div className="text-sm text-text-primary">Push notifications on this browser</div>
        <div className="text-xs text-text-dim">{detail}</div>
      </div>
      {action}
    </div>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="px-4 py-2.5 flex items-center gap-6">
      <span className="w-32 text-sm text-text-secondary">{label}</span>
      <span className="font-mono text-sm text-text-primary truncate">{children}</span>
    </div>
  );
}

function signIn(about: ServerAbout | null): string {
  if (!about) return "Unknown";
  if (about.passphrase_enabled) return "Passphrase required";
  return about.auth_required ? "Token in the URL" : "None";
}

export function AccessStep({ status, about, schema, values, onSaveField }: Props) {
  return (
    <>
      <Section title="Notifications">
        <div className={PANEL}>
          <PushRow />
          <div className="p-4">
            <SchemaSection
              section="web"
              schema={schema}
              values={values}
              onSaveField={onSaveField}
              onlyFields={["notify_on_waiting", "notify_on_idle", "notify_on_error"]}
            />
          </div>
        </div>
      </Section>
      <Section title="How this dashboard is reached" action={<span className="text-xs text-text-dim">Read only</span>}>
        <div className={PANEL}>
          <Row label="This address">{window.location.origin}</Row>
          {status.access.tunnel_url && <Row label="Tunnel">{status.access.tunnel_url}</Row>}
          <Row label="Sign-in">{signIn(about)}</Row>
        </div>
        <p className="text-xs text-text-dim">
          <a
            href="https://www.agent-of-empires.com/docs/guides/web-dashboard"
            target="_blank"
            rel="noreferrer"
            className="text-brand-500 underline"
          >
            How to access from your phone
          </a>{" "}
          Change these with <span className="font-mono">aoe serve</span> flags.
        </p>
      </Section>
    </>
  );
}
