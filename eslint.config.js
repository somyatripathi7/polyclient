import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";
import tseslint from "typescript-eslint";
import prettier from "eslint-config-prettier";

export default tseslint.config(
  // Build output, vendored assets, and the Rust side are not linted here.
  {
    ignores: [
      "dist",
      "build",
      "node_modules",
      "src-tauri",
      "dev-plugins",
      "docs",
      "target",
    ],
  },
  {
    files: ["**/*.{ts,tsx}"],
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    languageOptions: {
      ecmaVersion: 2021,
      sourceType: "module",
      globals: globals.browser,
    },
    plugins: {
      "react-hooks": reactHooks,
      "react-refresh": reactRefresh,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      "react-refresh/only-export-components": [
        "warn",
        { allowConstantExport: true },
      ],
      // The plugin pipe is intentionally untyped JSON at the boundary; flag
      // `any` as a warning rather than blocking the build on it.
      "@typescript-eslint/no-explicit-any": "warn",
      // Allow `_`-prefixed unused args/vars (matches the tsconfig convention).
      "@typescript-eslint/no-unused-vars": [
        "error",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
    },
  },
  // Disable stylistic rules that would conflict with Prettier.
  prettier,
);
