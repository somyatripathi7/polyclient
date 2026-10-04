// Curl-command parser for the `curlui` workspace's "import from curl" feature.
//
// This is a straight port of the Rust `parse_curl_command` (and its helpers)
// from `crates/plugins/curlui/src/lib.rs`, moved to the frontend so importing a
// curl command no longer spawns the sidecar. It is a purpose-built, shell-aware
// parser (not a strict grammar): it tolerates browser "Copy as cURL" output by
// tokenizing with quote/escape/line-continuation handling, interpreting the
// flags we care about, and ignoring the rest. Keep it behaviourally in sync with
// the Rust unit tests in that file.

import { base64Utf8, type HttpRequest, type MultipartPart } from "./curlui.ts";

/** Parse a `curl` command line into an {@link HttpRequest}. Throws if no URL is
 *  found or a quote is left unterminated. */
export function parseCurl(curl: string): HttpRequest {
  const tokens = tokenizeShell(curl);
  let i = 0;
  const peek = (): string | undefined => (i < tokens.length ? tokens[i] : undefined);
  const next = (): string | undefined => (i < tokens.length ? tokens[i++] : undefined);

  // Skip an optional leading `curl` (and a shell prompt like `$`). Anything else
  // (a flag or a bare URL) is treated as the start of the args.
  const first = peek();
  if (first !== undefined && (first === "$" || first.toLowerCase() === "curl")) {
    next();
  }

  let method: string | undefined;
  let url: string | undefined;
  const headers: Record<string, string> = {};
  const data: string[] = [];
  const forms: MultipartPart[] = [];
  let basicAuth: string | undefined;

  while (i < tokens.length) {
    const tok = next() as string;
    const [name, inline] = splitFlag(tok);
    // Flags that take a value: use the inline `=value`, else the next token.
    const valueFlag = (): string | undefined => (inline !== null ? inline : next());

    switch (name) {
      case "-X":
      case "--request": {
        const m = valueFlag();
        if (m !== undefined) method = m.trim().toUpperCase();
        break;
      }
      case "-H":
      case "--header": {
        const h = valueFlag();
        if (h !== undefined) {
          const idx = h.indexOf(":");
          if (idx >= 0) {
            const k = h.slice(0, idx).trim();
            if (k) headers[k] = h.slice(idx + 1).trim();
          }
        }
        break;
      }
      case "-d":
      case "--data":
      case "--data-raw":
      case "--data-ascii":
      case "--data-binary":
      case "--data-urlencode": {
        // `--data-raw` keeps a leading `$`/`@` literally; we don't expand @file.
        const d = valueFlag();
        if (d !== undefined) data.push(d);
        break;
      }
      case "-F":
      case "--form":
      case "--form-string": {
        // `--form-string` is always a literal text field, even with a leading @/<.
        const f = valueFlag();
        if (f !== undefined) forms.push(parseFormField(f, name === "--form-string"));
        break;
      }
      case "-u":
      case "--user":
        basicAuth = valueFlag();
        break;
      case "-A":
      case "--user-agent": {
        const ua = valueFlag();
        if (ua !== undefined && !("User-Agent" in headers)) headers["User-Agent"] = ua;
        break;
      }
      case "-b":
      case "--cookie": {
        const c = valueFlag();
        if (c !== undefined && !("Cookie" in headers)) headers["Cookie"] = c;
        break;
      }
      case "-e":
      case "--referer": {
        const r = valueFlag();
        if (r !== undefined && !("Referer" in headers)) headers["Referer"] = r;
        break;
      }
      case "-L":
      case "--location":
      case "--url": {
        // `--url` takes the URL as its value; `-L` is a bare flag in real curl,
        // but some pasted commands put the URL right after it.
        if (name === "--url") {
          const u = valueFlag();
          if (u !== undefined) url = u;
        }
        break;
      }
      // Known value-less flags we can safely ignore.
      case "-k":
      case "--insecure":
      case "--compressed":
      case "-s":
      case "--silent":
      case "-i":
      case "--include":
      case "-v":
      case "--verbose":
      case "-S":
      case "--show-error":
      case "-f":
      case "--fail":
      case "-G":
      case "--get":
      case "-#":
      case "--progress-bar":
      case "-g":
      case "--globoff":
        break;
      default: {
        if (name.startsWith("-")) {
          // Unknown flag. If it clearly takes a value (long form without inline
          // `=`), consume the following token so it isn't mistaken for the URL.
          // Short unknown flags are assumed value-less.
          if (name.startsWith("--") && inline === null) {
            const nx = peek();
            if (nx !== undefined && !nx.startsWith("-") && url !== undefined) {
              // URL already found; the trailing token is this flag's value.
              next();
            }
          }
        } else if (url === undefined) {
          // Positional token → the URL (first one wins).
          url = tok;
        }
      }
    }
  }

  if (url === undefined) throw new Error("curl: no URL found");

  if (basicAuth !== undefined && !("Authorization" in headers)) {
    headers["Authorization"] = `Basic ${base64Utf8(basicAuth)}`;
  }

  let body: string | null = data.length === 0 ? null : data.join("&");
  let bodyKind: HttpRequest["body_kind"];
  let parts: MultipartPart[] | undefined;
  // `-F` makes it a multipart request; that takes precedence over `-d` data.
  if (forms.length > 0) {
    body = null;
    bodyKind = "multipart";
    parts = forms;
  } else {
    bodyKind = inferBodyKind(headers, body);
  }

  const method_ = method ?? (body !== null || parts !== undefined ? "POST" : "GET");

  return {
    method: method_,
    url,
    headers,
    body,
    body_kind: bodyKind,
    ...(parts !== undefined ? { parts } : {}),
  };
}

/** Parse one `-F`/`--form` field value into a {@link MultipartPart}. curl
 *  syntax: `name=value` is a text field; `name=@path` / `name=<path` is a file
 *  field, with optional `;type=<mime>` and `;filename=<name>` modifiers.
 *  `forceText` (from `--form-string`) keeps the value literal. */
function parseFormField(field: string, forceText: boolean): MultipartPart {
  const eq = field.indexOf("=");
  const name = eq >= 0 ? field.slice(0, eq).trim() : field.trim();
  const rhs = eq >= 0 ? field.slice(eq + 1) : "";

  const isFile = !forceText && (rhs.startsWith("@") || rhs.startsWith("<"));
  if (!isFile) {
    return { name, kind: "text", value: rhs, filename: null, content_type: null };
  }

  // Split the path from any `;type=`/`;filename=` modifiers.
  const segs = rhs.slice(1).split(";");
  const path = segs.length > 0 ? segs[0] : "";
  let content_type: string | null = null;
  let filename: string | null = null;
  for (let j = 1; j < segs.length; j++) {
    const seg = segs[j].trim();
    if (seg.startsWith("type=")) content_type = seg.slice("type=".length);
    else if (seg.startsWith("filename=")) filename = seg.slice("filename=".length);
  }
  return { name, kind: "file", value: path, filename, content_type };
}

/** Split a flag token into its name and optional inline value (`--data=x`).
 *  Returns `null` for the value when there is no inline `=` (distinct from an
 *  empty-string value like `--data=`). */
function splitFlag(tok: string): [string, string | null] {
  if (tok.startsWith("--")) {
    const eq = tok.indexOf("=");
    if (eq >= 0) return [tok.slice(0, eq), tok.slice(eq + 1)];
  }
  return [tok, null];
}

function inferBodyKind(
  headers: Record<string, string>,
  body: string | null,
): HttpRequest["body_kind"] {
  if (body === null || body === "") return "none";
  for (const [k, v] of Object.entries(headers)) {
    if (k.toLowerCase() === "content-type") {
      if (v.includes("json")) return "json";
      if (v.includes("x-www-form-urlencoded")) return "form";
    }
  }
  return "text";
}

/** Tokenize a shell-style command line: split on whitespace, honor single and
 *  double quotes, backslash escapes, and `\`-newline line continuations.
 *  Throws on an unterminated double quote. */
function tokenizeShell(input: string): string[] {
  const tokens: string[] = [];
  let cur = "";
  let hasToken = false;
  const chars = Array.from(input);
  let i = 0;

  while (i < chars.length) {
    const c = chars[i++];
    if (c === " " || c === "\t" || c === "\r" || c === "\n") {
      if (hasToken) {
        tokens.push(cur);
        cur = "";
        hasToken = false;
      }
    } else if (c === "\\") {
      const nx = i < chars.length ? chars[i++] : undefined;
      // Line continuation: swallow the newline, no token started.
      if (nx === "\n" || nx === "\r") {
        // no-op
      } else if (nx !== undefined) {
        hasToken = true;
        cur += nx;
      }
    } else if (c === "'") {
      hasToken = true;
      while (i < chars.length) {
        const q = chars[i++];
        if (q === "'") break;
        cur += q;
      }
    } else if (c === '"') {
      hasToken = true;
      let closed = false;
      while (i < chars.length) {
        const q = chars[i++];
        if (q === '"') {
          closed = true;
          break;
        }
        if (q === "\\") {
          // In double quotes, backslash escapes a few chars.
          const e = i < chars.length ? chars[i++] : undefined;
          if (e === '"' || e === "\\" || e === "$" || e === "`") cur += e;
          else if (e !== undefined) cur += "\\" + e;
        } else {
          cur += q;
        }
      }
      if (!closed) throw new Error("unterminated double quote");
    } else {
      hasToken = true;
      cur += c;
    }
  }
  if (hasToken) tokens.push(cur);
  return tokens;
}
