import { useState, useRef, useEffect } from "react";
import { login } from "../lib/api";

interface Props {
  onSuccess: () => void;
}

export function LoginPage({ onSuccess }: Props) {
  const [passphrase, setPassphrase] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [showPassphrase, setShowPassphrase] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (loading || !passphrase.trim()) return;

    setLoading(true);
    setError(null);

    const result = await login(passphrase);

    if (result.ok) {
      onSuccess();
    } else {
      setError(result.error ?? "Login failed");
      setLoading(false);
      inputRef.current?.focus();
    }
  };

  return (
    <div className="h-dvh flex items-center justify-center bg-surface-900 p-4 safe-area-inset">
      <div className="w-full max-w-sm animate-slide-up">
        <form onSubmit={handleSubmit} className="bg-surface-800 border border-surface-700/40 rounded-xl p-8">
          {/* Logo */}
          <div className="flex items-center justify-center gap-2 mb-8">
            <img src="/icon-192.png" alt="" width="28" height="28" className="rounded-sm" />
            <span className="font-mono text-lg text-text-primary tracking-tight">aoe</span>
          </div>

          {/* Passphrase input */}
          <div className="mb-4">
            <label htmlFor="passphrase" className="block text-xs text-text-muted mb-2 font-medium">
              Passphrase
            </label>
            <div className="relative">
              <input
                ref={inputRef}
                id="passphrase"
                type={showPassphrase ? "text" : "password"}
                value={passphrase}
                onChange={(e) => setPassphrase(e.target.value)}
                disabled={loading}
                autoComplete="current-password"
                className="w-full px-3 py-2.5 pr-10 bg-surface-900 border border-surface-700/60 rounded-lg text-text-primary text-sm placeholder:text-text-dim focus:outline-none focus:ring-2 focus:ring-brand-600 focus:border-transparent disabled:opacity-50 transition-colors"
                placeholder="Enter passphrase"
              />
              <button
                type="button"
                onClick={() => setShowPassphrase((s) => !s)}
                className="absolute right-2 top-1/2 -translate-y-1/2 w-7 h-7 flex items-center justify-center text-text-dim hover:text-text-secondary transition-colors cursor-pointer rounded"
                tabIndex={-1}
                aria-label={showPassphrase ? "Hide passphrase" : "Show passphrase"}
              >
                {showPassphrase ? (
                  <svg
                    width="16"
                    height="16"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="1.5"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94" />
                    <path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19" />
                    <line x1="1" y1="1" x2="23" y2="23" />
                    <path d="M14.12 14.12a3 3 0 1 1-4.24-4.24" />
                  </svg>
                ) : (
                  <svg
                    width="16"
                    height="16"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="1.5"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
                    <circle cx="12" cy="12" r="3" />
                  </svg>
                )}
              </button>
            </div>
          </div>

          {/* Error message */}
          {error && <p className="text-status-error text-xs mb-4">{error}</p>}

          {/* Submit button */}
          <button
            type="submit"
            disabled={loading || !passphrase.trim()}
            className="w-full py-2.5 bg-brand-600 hover:bg-brand-700 text-white text-sm font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer flex items-center justify-center gap-2"
          >
            {loading ? (
              <>
                <svg className="animate-spin h-4 w-4" viewBox="0 0 24 24" fill="none">
                  <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                  <path
                    className="opacity-75"
                    fill="currentColor"
                    d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
                  />
                </svg>
                Signing in...
              </>
            ) : (
              "Sign in"
            )}
          </button>
        </form>
      </div>
    </div>
  );
}
