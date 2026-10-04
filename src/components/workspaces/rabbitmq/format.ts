/** Format a per-second rate (e.g. message publish rate). */
export const fmtRate = (r: number | undefined) => `${(r ?? 0).toFixed(1)}/s`;

/** Human-readable byte size (binary units). */
export function fmtBytes(n: number): string {
  if (!n) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

/** The default vhost "/" displays as "/" but is its own selectable value. */
export const vhostLabel = (v: string) => v || "/";
