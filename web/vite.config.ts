/// <reference types="vitest/config" />
import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { codecovVitePlugin } from "@codecov/vite-plugin";

export const SESSION_WS_PROXY = "^/sessions/.+/(?:ws|live-ws)(?:\\?.*)?$";

export default defineConfig(({ mode, command }) => {
  const env = loadEnv(mode, process.cwd(), "");

  const collectCoverage = env.AOE_COVERAGE === "1";

  const enableBundleAnalysis = command === "build" && !collectCoverage && !!env.CODECOV_TOKEN;

  // VITE_PROXY points `npm run dev` at a running `aoe serve`; read here only so it isn't bundled.
  const httpTarget = (() => {
    const raw = env.VITE_PROXY?.trim();
    if (!raw) return null;
    return /^https?:\/\//.test(raw) ? raw : `http://${raw}`;
  })();

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
      // Must come last so it sees the final bundle.
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
      // External maps by default: inline maps slow every Playwright navigation.
      // The live suite opts into inline because `aoe serve` embeds dist/ and serves no .map files.
      sourcemap: collectCoverage ? (env.AOE_COVERAGE_INLINE_SOURCEMAP === "1" ? "inline" : true) : false,
    },
    test: {
      include: ["src/**/*.{test,spec}.{ts,tsx}", "tests/helpers/**/*.test.ts"],
      exclude: ["tests/**/*.spec.{ts,tsx}", "node_modules/**", "dist/**", "src/**/*.types.test.ts"],
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
