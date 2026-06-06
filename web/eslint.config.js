import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";
import youMightNotNeedAnEffect from "eslint-plugin-react-you-might-not-need-an-effect";
import tseslint from "typescript-eslint";
import { defineConfig, globalIgnores } from "eslint/config";

export default defineConfig([
  globalIgnores(["dist"]),
  {
    files: ["**/*.{ts,tsx}"],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
    rules: {
      // Playwright tests destructure an empty fixture bag (`({}, testInfo) => ...`)
      // to reach the second argument. v10's no-empty-pattern flags this; allow it.
      "no-empty-pattern": ["error", { allowObjectPatternsAsParameters: true }],
      "@typescript-eslint/no-unused-vars": [
        "error",
        {
          argsIgnorePattern: "^_",
          varsIgnorePattern: "^_",
          destructuredArrayIgnorePattern: "^_",
          caughtErrorsIgnorePattern: "^_",
        },
      ],
      // Deferred from the v10 upgrade (react-hooks v7 compiler-aware rules
      // plus react-refresh's tightened only-export-components). Now enabled at
      // error severity with their pre-existing violations frozen in
      // eslint-suppressions.json (immutability 1, set-state-in-effect 30,
      // only-export-components 22). New violations fail; burn the suppressions
      // down with `eslint --prune-suppressions`. set-state-in-effect overlaps
      // eslint-plugin-react-you-might-not-need-an-effect, so its count will
      // shrink as that suppression set is cleared.
      "react-hooks/set-state-in-effect": "error",
      "react-hooks/immutability": "error",
      "react-refresh/only-export-components": "error",
    },
  },
  {
    // Test specs match ANSI escape codes in regexes by design (terminal output).
    // Playwright fixture callbacks use `use(value)`; eslint-plugin-react-hooks v7
    // misidentifies these as the React `use` hook.
    files: [
      "tests/**/*.{ts,tsx}",
      "src/**/*.test.{ts,tsx}",
      "src/**/__tests__/**",
    ],
    rules: {
      "no-control-regex": "off",
      "react-hooks/rules-of-hooks": "off",
    },
  },
  {
    // Ban bare localStorage.setItem in production source. All non-critical
    // writes must route through safeSetItem in src/lib/safeStorage.ts so
    // QuotaExceededError, SecurityError, and private-mode throws stay
    // swallowed. Exceptions (token.ts, deviceBinding.ts) have inline
    // `eslint-disable-next-line no-restricted-syntax` annotations
    // documenting their deliberate rethrow contracts. See
    // docs/development/web-storage.md and #1345.
    files: ["src/**/*.{ts,tsx}"],
    ignores: ["src/**/*.test.{ts,tsx}", "src/**/__tests__/**"],
    rules: {
      "no-restricted-syntax": [
        "error",
        {
          selector:
            "CallExpression[callee.object.name='localStorage'][callee.property.name='setItem']",
          message:
            "Use safeSetItem from src/lib/safeStorage.ts instead of bare localStorage.setItem.",
        },
        {
          selector:
            "CallExpression[callee.object.object.name='window'][callee.object.property.name='localStorage'][callee.property.name='setItem']",
          message:
            "Use safeSetItem from src/lib/safeStorage.ts instead of bare window.localStorage.setItem.",
        },
      ],
    },
  },
  {
    // Flag effects that React doesn't need (derived state, event-handler logic,
    // prop-sync, etc.). Enabled at error severity, but the ~89 pre-existing
    // violations are frozen in eslint-suppressions.json so they don't block
    // CI; new violations fail. Burn the suppressions down file-by-file with
    // `eslint --prune-suppressions`, then delete the file once empty.
    // See https://react.dev/learn/you-might-not-need-an-effect.
    ...youMightNotNeedAnEffect.configs.strict,
    files: ["src/**/*.{ts,tsx}"],
    ignores: ["src/**/*.test.{ts,tsx}", "src/**/__tests__/**"],
  },
]);
