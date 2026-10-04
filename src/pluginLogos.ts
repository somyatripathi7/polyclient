// Plugin logos, bundled as frontend assets.
//
// Logos live in `src/assets/plugin-logos/` as `<name>.svg` and are collected at
// build time via Vite's `import.meta.glob`. Resolution prefers a logo matching
// the plugin *id* (e.g. `postgres.svg`), then falls back to one matching the
// plugin *kind* (e.g. `rdbms.svg`). When neither exists the caller should fall
// back to the colored connection dot.
//
// To add a logo for a new plugin, drop `<id>.svg` (or a `<kind>.svg` shared by
// a whole category) into the directory — no code change needed.

// Eagerly resolve every logo to its final bundled URL. Keyed by bare file name
// without extension (e.g. "postgres", "rdbms").
const modules = import.meta.glob("./assets/plugin-logos/*.svg", {
  eager: true,
  query: "?url",
  import: "default",
}) as Record<string, string>;

const logos: Record<string, string> = {};
for (const [path, url] of Object.entries(modules)) {
  const name = path
    .split("/")
    .pop()!
    .replace(/\.svg$/, "");
  logos[name] = url;
}

/** Resolve a plugin's logo URL: by id first, then by kind. Returns null when
 *  no bundled logo matches (caller falls back to the colored dot). */
export function pluginLogo(pluginId: string, kind: string): string | null {
  return logos[pluginId] ?? logos[kind] ?? null;
}
