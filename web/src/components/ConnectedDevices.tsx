import { useEffect, useState } from "react";
import { fetchDevices, revokeDevice, signOutAllDevices, type DeviceSession } from "../lib/api";

/** Parse a raw user-agent string into a short "Browser + OS" label. */
function parseUserAgent(ua: string): string {
  let browser = "Unknown";
  let os = "Unknown";

  if (ua.includes("Firefox/")) browser = "Firefox";
  else if (ua.includes("Edg/")) browser = "Edge";
  else if (ua.includes("Chrome/")) browser = "Chrome";
  else if (ua.includes("Safari/")) browser = "Safari";
  else if (ua.includes("curl/")) browser = "curl";

  if (ua.includes("iPhone") || ua.includes("iPad")) os = "iOS";
  else if (ua.includes("Android")) os = "Android";
  else if (ua.includes("Mac OS X")) os = "macOS";
  else if (ua.includes("Windows")) os = "Windows";
  else if (ua.includes("Linux")) os = "Linux";

  return `${browser} · ${os}`;
}

/** Format a timestamp as a relative "last seen" string. */
function relativeTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const secs = Math.floor(diff / 1000);
  if (secs < 10) return "just now";
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/** Whether a device was active in the last 60 seconds. */
function isActive(iso: string): boolean {
  return Date.now() - new Date(iso).getTime() < 60_000;
}

/** Whether a device has been inactive for more than 1 hour. */
function isInactive(iso: string): boolean {
  return Date.now() - new Date(iso).getTime() > 3_600_000;
}

const Heading = () => (
  <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">Connected Devices</h3>
);

export function ConnectedDevices() {
  const [devices, setDevices] = useState<DeviceSession[] | null>(null);
  const [error, setError] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  const load = async () => {
    const result = await fetchDevices();
    if (result === null) {
      setError(true);
    } else {
      setError(false);
      setDevices(result);
    }
  };

  useEffect(() => {
    const interval = setInterval(load, 10_000);

    const onFocus = () => {
      if (document.visibilityState === "visible") load();
    };
    document.addEventListener("visibilitychange", onFocus);

    // Defer the first load so it's not called synchronously in the effect
    // body, satisfying set-state-in-effect.
    const first = setTimeout(load, 0);

    return () => {
      clearInterval(interval);
      clearTimeout(first);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, []);

  const onRevoke = async (sessionId: string) => {
    setBusy(sessionId);
    // A 403 here pops the global passphrase prompt (fetch interceptor);
    // revokeDevice resolves false and the user retries after confirming.
    const ok = await revokeDevice(sessionId);
    setBusy(null);
    if (ok) await load();
  };

  const onSignOutAll = async () => {
    if (!window.confirm("Sign out every device, including this one? You will need the passphrase to sign back in.")) {
      return;
    }
    setBusy("__all__");
    const ok = await signOutAllDevices();
    setBusy(null);
    if (ok) await load();
  };

  if (error) {
    return (
      <div>
        <Heading />
        <p className="font-body text-[13px] text-status-error">Could not load devices</p>
      </div>
    );
  }

  if (devices === null) {
    return (
      <div>
        <Heading />
        <p className="font-mono text-[11px] text-text-muted">Loading...</p>
      </div>
    );
  }

  if (devices.length === 0) {
    return (
      <div>
        <Heading />
        <div className="flex flex-col items-center py-8">
          <svg
            className="w-12 h-12 text-brand-600 mb-3"
            fill="none"
            viewBox="0 0 24 24"
            stroke="currentColor"
            strokeWidth={1.5}
          >
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              d="M9 12.75L11.25 15 15 9.75m-3-7.036A11.959 11.959 0 013.598 6 11.99 11.99 0 003 9.749c0 5.592 3.824 10.29 9 11.623 5.176-1.332 9-6.03 9-11.622 0-1.31-.21-2.571-.598-3.751h-.152c-3.196 0-6.1-1.248-8.25-3.285z"
            />
          </svg>
          <p className="font-body text-[16px] text-text-muted">No signed-in devices</p>
          <p className="font-body text-[13px] text-text-muted mt-1">
            Devices appear here after they sign in with the passphrase
          </p>
        </div>
      </div>
    );
  }

  return (
    <div>
      <Heading />
      <div>
        {devices.map((device, i) => (
          <div
            key={device.session_id}
            className={`flex items-center justify-between gap-3 py-3 ${
              i < devices.length - 1 ? "border-b border-surface-700" : ""
            }`}
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span
                  className={`inline-block w-1.5 h-1.5 rounded-full ${
                    isActive(device.last_seen)
                      ? "bg-status-running"
                      : isInactive(device.last_seen)
                        ? "bg-status-idle"
                        : "bg-status-waiting"
                  }`}
                />
                <span className="font-body text-[13px] font-medium text-text-primary">
                  {parseUserAgent(device.user_agent)}
                </span>
                {device.current && (
                  <span className="font-mono text-[11px] uppercase tracking-wider text-text-secondary">
                    this device
                  </span>
                )}
              </div>
              <p className="font-body text-[11px] text-text-secondary ml-3.5">{device.created_ip}</p>
              <p className="font-body text-[11px] text-text-muted ml-3.5">
                last seen: {relativeTime(device.last_seen)}
              </p>
            </div>
            {!device.current && (
              <button
                type="button"
                onClick={() => onRevoke(device.session_id)}
                disabled={busy !== null}
                className="shrink-0 font-mono text-[11px] uppercase tracking-wider text-status-error border border-status-error/40 rounded px-2 py-1 hover:bg-status-error/10 disabled:opacity-50"
              >
                {busy === device.session_id ? "..." : "Revoke"}
              </button>
            )}
          </div>
        ))}
      </div>
      <button
        type="button"
        onClick={onSignOutAll}
        disabled={busy !== null}
        className="mt-4 font-mono text-[11px] uppercase tracking-wider text-status-error border border-status-error/40 rounded px-3 py-1.5 hover:bg-status-error/10 disabled:opacity-50"
      >
        {busy === "__all__" ? "Signing out..." : "Sign out all devices"}
      </button>
    </div>
  );
}
