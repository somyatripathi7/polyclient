// UI themes.
//
// Each theme is a `[data-theme="<id>"]` block in `src/styles.css` that overrides
// the CSS custom properties (--bg, --text, --accent, …) the whole app is painted
// from. This module is just the registry the theme picker renders and the helper
// that flips the active theme. The selected id is persisted in AppConfig.theme.

/** A selectable theme. `id` must match a `[data-theme]` block in styles.css. */
export interface Theme {
  id: string;
  label: string;
  /** Whether the palette is light (used to group/sort in the picker). */
  light: boolean;
}

/** The 10 built-in themes, in display order. `mocha` is the default. */
export const THEMES: Theme[] = [
  { id: "mocha", label: "Catppuccin Mocha", light: false },
  { id: "dracula", label: "Dracula", light: false },
  { id: "nord", label: "Nord", light: false },
  { id: "tokyo-night", label: "Tokyo Night", light: false },
  { id: "gruvbox", label: "Gruvbox Dark", light: false },
  { id: "monokai", label: "Monokai", light: false },
  { id: "solarized-dark", label: "Solarized Dark", light: false },
  { id: "solarized-light", label: "Solarized Light", light: true },
  { id: "latte", label: "Catppuccin Latte", light: true },
  { id: "github-light", label: "GitHub Light", light: true },
];

/** The id used when config has no theme yet or names an unknown one. */
export const DEFAULT_THEME = "mocha";

/** Resolve an arbitrary string to a known theme id (falls back to default). */
export function resolveTheme(id: string | undefined | null): string {
  return THEMES.some((t) => t.id === id) ? (id as string) : DEFAULT_THEME;
}

/** Apply a theme by setting `data-theme` on <html>; styles.css does the rest. */
export function applyTheme(id: string | undefined | null): void {
  document.documentElement.setAttribute("data-theme", resolveTheme(id));
}
