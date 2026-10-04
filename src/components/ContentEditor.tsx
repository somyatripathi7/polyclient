import { useCallback, useMemo, useState } from "react";
import { CodeEditorV2, type CodeLanguage, type EditorKeybinding } from "./CodeEditorV2.tsx";

const INDENT = "  "; // two spaces per level — matches JSON.stringify(…, 2)

/** The editing flavors this component knows how to highlight/format. Anything
 *  unrecognized falls back to {@link Format.Text} (a plain editor). */
type Format = "json" | "xml" | "html" | "text";

/** Map a (possibly parameterized) MIME `Content-Type` onto a {@link Format}.
 *  Strips any `; charset=…` parameter and matches the common families plus the
 *  structured-syntax suffixes (`+json`, `+xml`), defaulting to plain text. */
function resolveFormat(contentType: string | undefined): Format {
  const mime = (contentType ?? "").split(";")[0].trim().toLowerCase();
  if (mime === "application/json" || mime === "text/json" || mime.endsWith("+json")) return "json";
  if (mime === "text/html" || mime === "application/xhtml+xml") return "html";
  if (mime === "application/xml" || mime === "text/xml" || mime.endsWith("+xml")) return "xml";
  return "text";
}

/** CodeMirror grammar for a format. Text has no grammar — it renders as a plain
 *  editor, so this is only consulted for the code formats. */
const FORMAT_LANGUAGE: Record<Exclude<Format, "text">, CodeLanguage> = {
  json: "json",
  xml: "xml",
  html: "html",
};

/** Fold macOS "smart quotes" (“ ” ‘ ’) back to straight quotes before parsing.
 *  The OS text-substitution swaps them in as you type, but they're never valid
 *  JSON, so a hand-typed object would fail to parse without this. */
function normalizeQuotes(s: string): string {
  return s.replace(/[“”„‟]/g, '"').replace(/[‘’‚‛]/g, "'");
}

/** Turn a `JSON.parse` SyntaxError into a verbose, line-anchored message.
 *  Engines phrase the location differently — V8 uses `at position N` (and
 *  newer builds add `(line L column C)`), Firefox uses `at line L column C`,
 *  WebKit (Tauri on macOS) often gives no location at all. We pull whatever
 *  offset we can and compute line:column from `src` ourselves so the result is
 *  consistent regardless of engine. */
function describeParseError(err: unknown, src: string): string {
  const raw = err instanceof Error ? err.message : String(err);

  // Prefer an explicit "line L column C" if the engine already gave one.
  const lc = raw.match(/line (\d+) column (\d+)/i);
  if (lc) return `${cleanMessage(raw)} (line ${lc[1]}, column ${lc[2]})`;

  // Otherwise derive line/column from a character offset ("position N").
  const pos = raw.match(/position (\d+)/i);
  if (pos) {
    const offset = Math.min(Number(pos[1]), src.length);
    const before = src.slice(0, offset);
    const line = before.split("\n").length;
    const column = offset - before.lastIndexOf("\n");
    return `${cleanMessage(raw)} (line ${line}, column ${column})`;
  }

  return cleanMessage(raw);
}

/** Strip the engine's own trailing location clause so we don't print it twice
 *  alongside our normalized "(line L, column C)" suffix, and drop V8's noisy
 *  "…" "<source snippet>" is not valid JSON tail. */
function cleanMessage(raw: string): string {
  return raw
    .replace(/,?\s*"[\s\S]*?"\s*is not valid JSON\s*$/i, "")
    .replace(/\s*in JSON at position \d+.*$/i, "")
    .replace(/\s*at line \d+ column \d+.*$/i, "")
    .replace(/\s*\(line \d+ column \d+\).*$/i, "")
    .trim();
}

/** Parse markup and surface the first parser error, or `null` if well-formed.
 *  XML is parsed strictly (`application/xml`, which yields a `<parsererror>`
 *  document on malformed input); HTML uses the lenient `text/html` parser,
 *  which recovers from almost anything, so this rarely fails for HTML. */
function markupError(src: string, format: "xml" | "html"): string | null {
  const mime = format === "xml" ? "application/xml" : "text/html";
  const doc = new DOMParser().parseFromString(src, mime);
  const err = doc.querySelector("parsererror");
  if (!err) return null;
  // Collapse the (often multi-line) browser parser-error text to one line.
  return err.textContent?.replace(/\s+/g, " ").trim() || "Malformed markup";
}

/** Best-effort pretty-printer for XML/HTML: put each tag on its own line and
 *  reindent by nesting depth. Operates on the raw string (not the DOM) so it
 *  preserves comments, processing instructions, and text runs as-authored;
 *  pure-whitespace gaps between tags are collapsed. Tags whose attribute
 *  values contain a literal `>` can confuse the tag boundaries — acceptable for
 *  a best-effort formatter, and callers validate first for XML. */
function formatMarkup(src: string): string {
  const normalized = src
    .replace(/\r\n?/g, "\n")
    .replace(/>\s+</g, "><") // drop whitespace-only gaps between tags
    .replace(/></g, ">\n<") // one tag (or text run) per line
    .trim();

  let depth = 0;
  const out: string[] = [];
  for (const raw of normalized.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const isClosing = /^<\//.test(line);
    // Self-closing tags, declarations (<?…?>, <!…>) and comments don't nest.
    const isVoid = /\/>$/.test(line) || /^<[?!]/.test(line);
    const isOpening = /^<[^/!?]/.test(line) && !isVoid;
    // A line that opens and closes its own tag (<a>text</a>) is depth-neutral.
    const selfContained = isOpening && /<\/[^>]+>\s*$/.test(line);

    if (isClosing) depth = Math.max(0, depth - 1);
    out.push(INDENT.repeat(depth) + line);
    if (isOpening && !selfContained) depth++;
  }
  return out.join("\n");
}

/** Pretty-print a value for read-only display, by format. Best-effort: returns
 *  the input unchanged if it doesn't parse (so a truncated/invalid body is
 *  still shown verbatim rather than blanked). */
function formatForDisplay(value: string, format: Format): string {
  if (!value.trim()) return value;
  try {
    if (format === "json") return JSON.stringify(JSON.parse(normalizeQuotes(value)), null, 2);
    if ((format === "xml" || format === "html") && markupError(value, format) === null)
      return formatMarkup(value);
  } catch {
    /* fall through to the raw value */
  }
  return value;
}

/** A code-oriented editor for structured request/response/cell bodies, keyed by
 *  a MIME `contentType`. Recognized families are highlighted and get
 *  Format/Validate actions:
 *  - `application/json` (and `*+json`): JSON pretty-print + syntax validation.
 *  - `application/xml`, `text/xml` (and `*+xml`): markup reindent + well-formed
 *    check (strict).
 *  - `text/html`, `application/xhtml+xml`: markup reindent + lenient parse.
 *  Anything else (including `text/plain`) renders as a plain editor with no
 *  toolbar.
 *
 *  Wraps the shared {@link CodeEditorV2} (CodeMirror) and adds the app-specific
 *  affordances on top. CodeMirror itself provides syntax highlighting, smart
 *  indent, bracket auto-close, and find (⌘/Ctrl+F); this component layers the
 *  Format/Validate toolbar and the read-only pretty-printed viewer.
 *
 *  `keybindings` are forwarded straight to the editor so a host can layer on
 *  shortcuts (Esc to cancel, ⌘/Ctrl+Enter to apply); they take precedence over
 *  the editor's own keymap.
 *  Everything funnels through `onChange`, so the parent stays the source of
 *  truth (no internal editor state, value stays controlled).
 *
 *  In `readOnly` mode (e.g. an HTTP response viewer) the value is pretty-printed
 *  for display, edits are disabled, and the Format/Validate toolbar is hidden —
 *  the surface stays selectable/scrollable but non-editable. */
export function ContentEditor({
  value,
  onChange,
  contentType,
  placeholder,
  className,
  autoFocus = false,
  readOnly = false,
  keybindings,
  env,
}: {
  value: string;
  onChange: (value: string) => void;
  contentType?: string;
  placeholder?: string;
  className?: string;
  autoFocus?: boolean;
  readOnly?: boolean;
  keybindings?: EditorKeybinding[];
  /** When set, `{{NAME}}` placeholders in the body highlight, colored by
   *  whether `NAME` resolves in this map. */
  env?: Record<string, string>;
}) {
  const format = resolveFormat(contentType);

  // In read-only mode the displayed text is pretty-printed; while editing it
  // must stay byte-for-byte the controlled value.
  const displayValue = useMemo(
    () => (readOnly ? formatForDisplay(value, format) : value),
    [readOnly, value, format],
  );

  // Inline status from Format/Validate; cleared on the next edit. `ok` drives
  // the message color (valid → success, invalid → error).
  const [status, setStatus] = useState<{ ok: boolean; message: string } | null>(null);

  const emit = useCallback(
    (next: string) => {
      setStatus(null);
      onChange(next);
    },
    [onChange],
  );

  /** Validate the current value for its format, returning the error message on
   *  failure (or `null` when valid). */
  const contentError = useCallback((): string | null => {
    if (!value.trim()) return "Empty body";
    if (format === "json") {
      const src = normalizeQuotes(value);
      try {
        JSON.parse(src);
        return null;
      } catch (e) {
        return describeParseError(e, src);
      }
    }
    // format is "xml" | "html" here ("text" never reaches the toolbar).
    return markupError(value, format as "xml" | "html");
  }, [value, format]);

  /** Reformat the body in place. JSON re-serializes with 2-space indent (folding
   *  smart quotes first); XML/HTML reindent by tag nesting. No-op (with an error
   *  message) if the body doesn't parse. */
  const reformat = useCallback(() => {
    if (!value.trim()) return;
    if (format === "json") {
      const src = normalizeQuotes(value);
      try {
        const pretty = JSON.stringify(JSON.parse(src), null, 2);
        if (pretty !== value) emit(pretty);
        setStatus({ ok: true, message: "Formatted" });
      } catch (e) {
        setStatus({ ok: false, message: describeParseError(e, src) });
      }
      return;
    }
    const err = markupError(value, format as "xml" | "html");
    if (err) {
      setStatus({ ok: false, message: err });
      return;
    }
    const pretty = formatMarkup(value);
    if (pretty !== value) emit(pretty);
    setStatus({ ok: true, message: "Formatted" });
  }, [value, format, emit]);

  const validate = useCallback(() => {
    const err = contentError();
    const okLabel =
      format === "json" ? "Valid JSON" : format === "xml" ? "Valid XML" : "Valid HTML";
    setStatus(err ? { ok: false, message: err } : { ok: true, message: okLabel });
  }, [contentError, format]);

  return (
    <div className={"content-editor" + (readOnly ? " is-readonly" : "")}>
      <CodeEditorV2
        className={className}
        language={format === "text" ? undefined : FORMAT_LANGUAGE[format]}
        value={displayValue}
        readOnly={readOnly}
        // Read-only keeps the value controlled by the parent (editor is
        // non-editable, so this never fires, but be explicit about intent).
        onChange={readOnly ? undefined : emit}
        placeholder={placeholder}
        autoFocus={autoFocus}
        keybindings={keybindings}
        highlightVars={readOnly ? undefined : env}
      />
      {!readOnly && format !== "text" && (
        <div className="content-editor-toolbar">
          <button type="button" onClick={reformat} title="Pretty-print (2-space indent)">
            Format
          </button>
          <button type="button" onClick={validate} title={`Check ${format.toUpperCase()} syntax`}>
            Validate
          </button>
          {status && (
            <span
              className={"content-editor-status " + (status.ok ? "is-ok" : "is-err")}
              title={status.message}
            >
              {status.message}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
