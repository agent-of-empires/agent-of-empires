import { useState, useRef, useEffect } from "react";
import { saveToken } from "../lib/token";
import { resetTokenExpired } from "../lib/fetchInterceptor";
import { verifyToken } from "../lib/api";

interface Props {
  onSuccess: () => void;
}

/** Extract a token from user input. Accepts either a raw 64-char hex token
 *  or a full dashboard URL containing `?token=<value>`. */
function extractToken(input: string): string {
  const trimmed = input.trim();
  try {
    const url = new URL(trimmed);
    const param = url.searchParams.get("token");
    if (param) return param;
  } catch {
    // Not a URL, treat as raw token
  }
  return trimmed;
}

export function TokenEntryPage({ onSuccess }: Props) {
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const token = extractToken(value);
    if (loading || !token) return;

    setLoading(true);
    setError(null);

    // Save to localStorage so the fetch interceptor attaches it as Bearer
    saveToken(token);
    resetTokenExpired();

    // /api/login/status is exempt from the passphrase session check, so a
    // token-good-but-passphrase-missing paste verifies as success here and
    // App.tsx routes to LoginPage. A session-gated endpoint would 401 and
    // look like a token rejection.
    const verified = await verifyToken();

    if (verified) {
      onSuccess();
    } else {
      // The interceptor already cleared localStorage on 401. Reset the
      // dedup flags so the next submission attempt can be detected too.
      resetTokenExpired();
      setError(
        "Invalid token. Copy the token from your `aoe serve` output and try again.",
      );
      setLoading(false);
      inputRef.current?.focus();
    }
  };

  return (
    <div className="h-dvh flex items-center justify-center bg-surface-900 p-4 safe-area-inset">
      <div className="w-full max-w-sm animate-slide-up">
        <form
          onSubmit={handleSubmit}
          className="bg-surface-800 border border-surface-700/40 rounded-xl p-8"
        >
          {/* Logo */}
          <div className="flex items-center justify-center gap-2 mb-6">
            <img
              src="/icon-192.png"
              alt=""
              width="28"
              height="28"
              className="rounded-sm"
            />
            <span className="font-mono text-lg text-text-primary tracking-tight">
              aoe
            </span>
          </div>

          {/* Explanation */}
          <p className="text-xs text-text-muted mb-6 text-center leading-relaxed">
            Your session token has expired or is missing. Paste the dashboard
            URL or token from{" "}
            <code className="text-brand-500 font-mono">aoe serve</code> to
            reconnect.
          </p>

          {/* Token input */}
          <div className="mb-4">
            <label
              htmlFor="token"
              className="block text-xs text-text-muted mb-2 font-medium"
            >
              Token or URL
            </label>
            <input
              ref={inputRef}
              id="token"
              type="text"
              value={value}
              onChange={(e) => setValue(e.target.value)}
              disabled={loading}
              autoComplete="off"
              spellCheck={false}
              className="w-full px-3 py-2.5 bg-surface-900 border border-surface-700/60 rounded-lg text-text-primary text-sm font-mono placeholder:text-text-dim focus:outline-none focus:ring-2 focus:ring-brand-600 focus:border-transparent disabled:opacity-50 transition-colors"
              placeholder="Paste token or URL"
            />
          </div>

          {/* Error message */}
          {error && <p className="text-status-error text-xs mb-4">{error}</p>}

          {/* Submit button */}
          <button
            type="submit"
            disabled={loading || !value.trim()}
            className="w-full py-2.5 bg-brand-600 hover:bg-brand-700 text-white text-sm font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer flex items-center justify-center gap-2"
          >
            {loading ? (
              <>
                <svg
                  className="animate-spin h-4 w-4"
                  viewBox="0 0 24 24"
                  fill="none"
                >
                  <circle
                    className="opacity-25"
                    cx="12"
                    cy="12"
                    r="10"
                    stroke="currentColor"
                    strokeWidth="4"
                  />
                  <path
                    className="opacity-75"
                    fill="currentColor"
                    d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
                  />
                </svg>
                Connecting...
              </>
            ) : (
              "Connect"
            )}
          </button>
        </form>
      </div>
    </div>
  );
}
