/** Fold macOS "smart quotes" (“ ” ‘ ’) back to straight quotes. The system
 * text-input substitution swaps them in as you type, but they're never valid
 * JSON, so a hand-typed object would fail to parse without this. */
export function normalizeQuotes(s: string): string {
  return s.replace(/[“”„‟]/g, '"').replace(/[‘’‚‛]/g, "'");
}

/** Character ranges of the top-level statements in `sql`, splitting on `;` and
 * ignoring `;` inside single-/double-quoted strings, dollar-quoted bodies, and
 * line/block comments. Mirrors the plugin-side splitter so "run statement at
 * cursor" matches what would actually execute. */
export function statementRanges(sql: string): { text: string; start: number; end: number }[] {
  const out: { text: string; start: number; end: number }[] = [];
  const n = sql.length;
  let i = 0;
  let start = 0;
  const push = (end: number) => {
    if (sql.slice(start, end).trim()) out.push({ text: sql.slice(start, end), start, end });
  };
  while (i < n) {
    const c = sql[i];
    if (c === "'" || c === '"') {
      const q = c;
      i++;
      while (i < n) {
        if (sql[i] === q) {
          if (sql[i + 1] === q) {
            i += 2;
            continue;
          }
          break;
        }
        i++;
      }
      i++;
    } else if (c === "-" && sql[i + 1] === "-") {
      i += 2;
      while (i < n && sql[i] !== "\n") i++;
    } else if (c === "/" && sql[i + 1] === "*") {
      i += 2;
      let depth = 1;
      while (i < n && depth > 0) {
        if (sql[i] === "/" && sql[i + 1] === "*") {
          depth++;
          i += 2;
        } else if (sql[i] === "*" && sql[i + 1] === "/") {
          depth--;
          i += 2;
        } else i++;
      }
    } else if (c === "$") {
      const m = /^\$([A-Za-z_]\w*)?\$/.exec(sql.slice(i));
      if (m) {
        const tag = m[0];
        i += tag.length;
        const close = sql.indexOf(tag, i);
        i = close < 0 ? n : close + tag.length;
      } else i++;
    } else if (c === ";") {
      push(i + 1);
      i++;
      start = i;
    } else i++;
  }
  push(n);
  return out;
}

/** The statement spanning `cursor` (the last statement starting at or before
 * the cursor), trimmed. Falls back to the whole trimmed script. */
export function statementAtCursor(sql: string, cursor: number): string {
  const ranges = statementRanges(sql);
  if (ranges.length === 0) return sql.trim();
  // Pick the first statement whose end is at or past the cursor. A cursor
  // sitting exactly on a boundary (right after a `;`) belongs to the statement
  // that just ended, not the next one.
  let chosen = ranges[ranges.length - 1];
  for (const r of ranges) {
    if (cursor <= r.end) {
      chosen = r;
      break;
    }
  }
  return chosen.text.trim();
}

/** Heuristically extract the single table a `SELECT` reads from, so its result
 * can be made editable. Returns null for anything we can't safely map to one
 * table: joins, unions, group-by, distinct, multiple tables, or subqueries.
 * An unqualified table name is assumed to live in `public`. */
export function parseSingleTable(query: string): { schema: string; table: string } | null {
  // Drop line comments and normalize whitespace.
  const q = query
    .replace(/--[^\n]*/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  // Multiple statements aren't a single editable table. (A `;` inside a string
  // literal would also bail out here — safe, just falls back to read-only.)
  if (q.replace(/;+\s*$/, "").includes(";")) return null;
  const lower = q.toLowerCase();
  if (!lower.startsWith("select ") && lower !== "select") return null;
  if (
    /\bjoin\b|\bunion\b|\bintersect\b|\bexcept\b|\bgroup\s+by\b|\bdistinct\b|\bhaving\b/.test(lower)
  ) {
    return null;
  }
  const fromMatch = lower.search(/\bfrom\b/);
  if (fromMatch < 0) return null;

  let rest = q.slice(fromMatch + 4).trim();
  // Cut the FROM clause at the next clause keyword (or statement end).
  const end = rest
    .toLowerCase()
    .search(/(\bwhere\b|\bgroup\b|\border\b|\blimit\b|\bhaving\b|\boffset\b|\bfetch\b|\bfor\b|;)/);
  if (end >= 0) rest = rest.slice(0, end).trim();
  // Reject multiple tables or a subquery in FROM.
  if (!rest || rest.includes(",") || rest.includes("(")) return null;

  // Match `schema.table` or `table`, each part optionally double-quoted; any
  // trailing alias is ignored.
  const m = rest.match(/^("[^"]+"|[A-Za-z_][\w$]*)(?:\s*\.\s*("[^"]+"|[A-Za-z_][\w$]*))?/);
  if (!m) return null;
  const unq = (s: string) => (s.startsWith('"') ? s.slice(1, -1) : s);
  return m[2] ? { schema: unq(m[1]), table: unq(m[2]) } : { schema: "public", table: unq(m[1]) };
}
