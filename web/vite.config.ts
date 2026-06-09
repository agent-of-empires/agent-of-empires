/// <reference types="vitest/config" />
import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import istanbul from "vite-plugin-istanbul";

export default defineConfig(({ mode }) => {
  // Load `.env*` files (empty prefix => all keys, not just `VITE_`), merged
  // over shell env. Editing a `.env` file restarts the dev server, and the
  // proxy below only intercepts `/api` + `/sessions/*/ws`, so Vite's own HMR
  // socket is untouched: live reload keeps working.
  const env = loadEnv(mode, process.cwd(), "");

  const collectCoverage = env.AOE_COVERAGE === "1";

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

  // All WebSocket routes live under `/sessions/{id}/...ws` (terminal,
  // container-terminal, and structured view at `/sessions/{id}/acp/ws`), so one
  // regex covers them; REST (including `/api/acp/*`) goes through `/api`.
  const proxy = httpTarget
    ? {
        "/api": { target: httpTarget, changeOrigin: true },
        "^/sessions/.+/ws": {
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
      ...(collectCoverage
        ? [
            istanbul({
              include: "src/**/*",
              exclude: ["node_modules", "dist", "**/*.test.{ts,tsx}", "**/__tests__/**"],
              extension: [".ts", ".tsx"],
              requireEnv: false,
              forceBuildInstrument: true,
            }),
          ]
        : []),
    ],
    build: {
      outDir: "dist",
      emptyOutDir: true,
      chunkSizeWarningLimit: 1500,
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
