/// <reference types="vitest/config" />
import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { codecovVitePlugin } from "@codecov/vite-plugin";

export const SESSION_WS_PROXY = "^/sessions/.+/(?:ws|live-ws)(?:\\?.*)?$";

export default defineConfig(({ mode, command }) => {
  // Load `.env*` files (empty prefix => all keys, not just `VITE_`), merged
  // over shell env. Editing a `.env` file restarts the dev server, and the
  // proxy below only intercepts `/api` + AoE `/sessions/*` WebSocket relays,
  // so Vite's own HMR socket is untouched: live reload keeps working.
  const env = loadEnv(mode, process.cwd(), "");

  const collectCoverage = env.AOE_COVERAGE === "1";

  // Codecov bundle analysis. Only on a real production build (`vite build`),
  // never on the coverage build (inline sourcemaps inflate chunk sizes and
  // would report bogus bundle stats) or in dev/test. Upload is gated on
  // CODECOV_TOKEN, so a local `npm run build` without the token is a no-op
  // rather than a failed upload.
  const enableBundleAnalysis = command === "build" && !collectCoverage && !!env.CODECOV_TOKEN;

  // Point `npm run dev` at an arbitrary running `aoe serve` (e.g. a released
  // binary on a non-default port) instead of a local cargo build. Set
  // VITE_PROXY to the server's origin (`localhost:50106` or
  // `http://localhost:50106`); unset means no proxy. Read only here (never
  // via import.meta.env), so it isn't bundled into the client.
  const httpTarget = (() => {
    const raw = env.VITE_PROXY?.trim();
    if (!raw) return null;
    return /^https?:\/\//.test(raw) ? raw : `http://${raw}`;
  })();

  // All AoE WebSocket routes live under `/sessions/{id}/` and include `ws`
  // suffixes (`ws`, `acp/ws`) plus the capture-snapshot live view
  // `live-ws` routes. One regex covers them; REST (including `/api/acp/*`)
  // goes through `/api`.
  const proxy = httpTarget
    ? {
        "/api": { target: httpTarget, changeOrigin: true },
        [SESSION_WS_PROXY]: {
          target: httpTarget.replace(/^http/, "ws"),
          ws: true,
          changeOrigin: true,
        },
      }
    : undefined;

  return {
    server: { proxy },
    plugins: [
      react(),
      tailwindcss(),
      // Must come last so it sees the final bundle. Inert unless
      // `enableBundleAnalysis` is true (see gating above).
      codecovVitePlugin({
        enableBundleAnalysis,
        bundleName: "agent-of-empires-web",
        uploadToken: env.CODECOV_TOKEN,
        gitService: "github",
      }),
    ],
    build: {
      outDir: "dist",
      emptyOutDir: true,
      chunkSizeWarningLimit: 1500,
      // Coverage builds keep production minification/chunking (so Playwright
      // exercises the real shipped bundle) but emit sourcemaps so monocart can
      // remap raw Chromium V8 byte ranges back to web/src, matching vitest's
      // v8 line map. (#2157)
      //
      // Inline vs external matters a lot for test wall-clock, because an inline
      // map is base64 inside the `.js` and every `page.goto` downloads and skips
      // it: the entry chunk goes from 1.73 MB to 10.65 MB, and a navigation with
      // V8 coverage on measures 332ms instead of 92ms. The mocked suite performs
      // ~400 navigations, so that is minutes.
      //
      // Default is therefore EXTERNAL. `AOE_COVERAGE_INLINE_SOURCEMAP=1` opts
      // back into inline for the one consumer that needs it: the live Playwright
      // suite runs against `aoe serve`, whose build.rs embeds `dist/` into the
      // binary via rust-embed, and a separate `.map` file has no serving path
      // there. `scripts/merge-coverage.mjs` reads either layout.
      sourcemap: collectCoverage ? (env.AOE_COVERAGE_INLINE_SOURCEMAP === "1" ? "inline" : true) : false,
    },
    // Vitest unit tests live alongside source as `*.test.ts(x)`. Playwright
    // suites under `tests/` use the same `.spec.ts` extension Playwright
    // expects but aren't valid vitest tests, so we explicitly exclude them.
    test: {
      include: ["src/**/*.{test,spec}.{ts,tsx}"],
      // Type-level tests (`*.types.test.ts`) run under the typecheck runner
      // below, not the runtime runner, so keep them out of `include`.
      exclude: ["tests/**", "node_modules/**", "dist/**", "src/**/*.types.test.ts"],
      // `expectTypeOf` assertions in `*.types.test.ts` are checked by tsc.
      // A failing assertion surfaces as a type error. Scoped to the
      // dedicated type-test files so the rest of the suite stays fast.
      typecheck: {
        enabled: true,
        include: ["src/**/*.types.test.ts"],
        tsconfig: "./tsconfig.vitest.json",
      },
      setupFiles: ["./src/test-setup.ts"],
      coverage: {
        provider: "v8",
        reporter: ["text", "json", "html", "lcov"],
        reportsDirectory: "./coverage/vitest",
        include: ["src/**/*.{ts,tsx}"],
        exclude: [
          "src/**/*.d.ts",
          "src/main.tsx",
          "src/test-setup.ts",
          "src/**/__tests__/**",
          "src/**/*.test.{ts,tsx}",
        ],
      },
    },
  };
});
