import { usePushSubscription } from "../hooks/usePushSubscription";

// Push-notifications settings section. Rendered inside SettingsView.
// State machine lives in usePushSubscription; this component is a view
// on top of it with a single Enable/Disable primary action and a
// secondary test-send button when enabled.

export function NotificationSettings() {
  const { state, enable, disable, sendTest, resubscribe } = usePushSubscription();

  const isBusy =
    state.kind === "asking" ||
    state.kind === "subscribing" ||
    state.kind === "sending-test" ||
    state.kind === "disabling" ||
    state.kind === "loading";

  return (
    <section className="rounded-lg border border-surface-700/50 bg-surface-900 p-4">
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-3">Notifications</h3>

      <StatusRow state={state} />

      <div className="mt-4 flex flex-wrap gap-2">
        {(state.kind === "off" || state.kind === "denied" || state.kind === "error") && (
          <button
            onClick={enable}
            disabled={isBusy}
            className="px-3 py-2 rounded-md bg-brand-600 hover:bg-brand-500 disabled:opacity-50 disabled:cursor-not-allowed text-sm font-medium text-surface-950 transition-colors"
          >
            {isBusy ? "Working..." : "Enable notifications"}
          </button>
        )}
        {state.kind === "enabled" && (
          <>
            <button
              onClick={sendTest}
              className="px-3 py-2 rounded-md bg-surface-700 hover:bg-surface-700/70 text-sm font-medium text-text-primary transition-colors"
            >
              Send test notification
            </button>
            <button
              onClick={resubscribe}
              title="Re-register this device. Use after changing the server port or hostname so notifications open the right URL."
              className="px-3 py-2 rounded-md border border-surface-700 hover:bg-surface-700/40 text-sm font-medium text-text-secondary transition-colors"
            >
              Re-subscribe
            </button>
            <button
              onClick={disable}
              className="px-3 py-2 rounded-md border border-surface-700 hover:bg-surface-700/40 text-sm font-medium text-text-secondary transition-colors"
            >
              Turn off
            </button>
          </>
        )}
      </div>

      {state.kind === "unsupported" && state.reason === "ios-not-standalone" && <IOSInstallHelp />}
    </section>
  );
}

function StatusRow({ state }: { state: ReturnType<typeof usePushSubscription>["state"] }) {
  switch (state.kind) {
    case "loading":
      return <p className="text-sm text-text-secondary">Checking...</p>;
    case "off":
      return (
        <p className="text-sm text-text-secondary">
          Off. Enable to receive a browser notification when an agent is waiting for your input.
        </p>
      );
    case "asking":
      return <p className="text-sm text-text-secondary">Asking your browser for permission...</p>;
    case "subscribing":
      return <p className="text-sm text-text-secondary">Registering device...</p>;
    case "enabled":
      return (
        <p className="text-sm text-status-running">
          Enabled. This device will get a lock-screen notification when an agent is waiting.
        </p>
      );
    case "sending-test":
      return (
        <p className="text-sm text-text-secondary">
          Sending test notification in a few seconds. On a phone, lock the screen now to see it land on the Lock Screen.
        </p>
      );
    case "disabling":
      return <p className="text-sm text-text-secondary">Turning off...</p>;
    case "denied":
      return (
        <p className="text-sm text-status-error">
          Permission was denied. Re-enable in your browser's site settings before turning this back on.
        </p>
      );
    case "disabled-by-server":
      return (
        <p className="text-sm text-text-secondary">
          Push notifications are turned off by the server. Contact the operator, or enable `web.notifications_enabled`
          in TUI settings.
        </p>
      );
    case "unsupported":
      if (state.reason === "insecure-origin") {
        return (
          <p className="text-sm text-text-secondary">
            Push notifications require HTTPS. On mobile, access this dashboard through a Cloudflare tunnel by running{" "}
            <code className="font-mono text-text-primary">aoe serve --remote</code> on your host, then open the printed
            URL on your phone.
          </p>
        );
      }
      if (state.reason === "ios-not-standalone") {
        return (
          <p className="text-sm text-text-secondary">
            Push notifications on iPhone require the app to be installed from Safari via Share, then Add to Home Screen.
            Open the installed app to enable.
          </p>
        );
      }
      return <p className="text-sm text-text-secondary">Your browser does not support Web Push.</p>;
    case "error":
      return <p className="text-sm text-status-error">Error: {state.message}</p>;
  }
}

function IOSInstallHelp() {
  return (
    <details className="mt-3 text-sm text-text-secondary">
      <summary className="cursor-pointer select-none">How to install on iPhone</summary>
      <ol className="mt-2 ml-5 list-decimal space-y-1">
        <li>Tap the Share icon at the bottom of Safari.</li>
        <li>
          Scroll down and tap <em>Add to Home Screen</em>.
        </li>
        <li>Open the app from your Home Screen (not Safari), then come back to this page.</li>
        <li>Tap Enable notifications.</li>
      </ol>
    </details>
  );
}
