import { readTextFile, writeTextFile } from "@tauri-apps/plugin-fs";
import {
  genId,
  listWorkspaceDir,
  readWorkspaceFile,
  writeWorkspaceFileAt,
  deleteWorkspacePath,
} from "./store.ts";

// --- HTTP Client (curlui) ---------------------------------------------------

export type BodyKind = "none" | "json" | "text" | "form" | "multipart";

/** One field of a `multipart/form-data` body. A `file` part carries a local
 *  file path in `value`; the (local sidecar) plugin reads the bytes at send
 *  time, so file contents never cross the host pipe. */
export interface MultipartPart {
  name: string;
  kind: "text" | "file";
  /** Text value for a `text` part, or the local file path for a `file` part. */
  value: string;
  /** Optional filename override for a `file` part (defaults to the basename). */
  filename?: string | null;
  /** Optional explicit Content-Type for the part. */
  content_type?: string | null;
}

export interface HttpRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  body?: string | null;
  body_kind: BodyKind;
  /** Multipart fields, used only when `body_kind === "multipart"`. */
  parts?: MultipartPart[];
}

export interface HttpResponse {
  status: number;
  status_text: string;
  headers: Record<string, string>;
  body: string;
  body_encoding: string;
  elapsed_ms: number;
}

export interface HttpFolder {
  id: string;
  name: string;
  folders: HttpFolder[];
  requests: HttpRequestItem[];
  /** Free-text (Markdown) documentation for this folder. */
  description?: string;
}

export interface HttpRequestItem {
  id: string;
  name: string;
  method: string;
  url: string;
  headers: Record<string, string>;
  body?: string | null;
  body_kind: BodyKind;
  /** Multipart fields, used only when `body_kind === "multipart"`. */
  parts?: MultipartPart[];
  auth?: Auth;
  /** Whether to merge the parent collection's headers (default true when
   *  unset). When false, only the request's own headers are sent. */
  inheritHeaders?: boolean;
  /** JavaScript run in a sandbox before the request is sent (a pre-request
   *  script). Has access to the `client` API (variables, request). */
  preScript?: string;
  /** JavaScript run in a sandbox after the response arrives. Runs `client.test`
   *  assertions and can save values from the response into variables. */
  postScript?: string;
  /** Free-text (Markdown) documentation for this request. */
  description?: string;
}

export type AuthKind = "inherit" | "none" | "bearer" | "basic" | "apikey";

/** Where an API-key credential is sent. */
export type ApiKeyIn = "header" | "query";

/** Per-request authorization config. All fields optional so
 *  partially-filled forms round-trip through persistence cleanly. `inherit`
 *  means "use the parent collection's auth"; `none` means explicitly no auth. */
export interface Auth {
  kind: AuthKind;
  /** bearer */
  token?: string;
  /** basic */
  username?: string;
  password?: string;
  /** apikey */
  key?: string;
  value?: string;
  in?: ApiKeyIn;
}

/** A request's default auth: inherit from the parent collection. */
export function defaultAuth(): Auth {
  return { kind: "inherit", in: "header" };
}

/** A collection's default auth: no auth (a collection has no parent to
 *  inherit from). */
export function defaultCollectionAuth(): Auth {
  return { kind: "none", in: "header" };
}

export interface HttpCollection {
  id: string;
  name: string;
  folders: HttpFolder[];
  requests: HttpRequestItem[];
  /** Collection-level `{{VAR}}` overrides, merged over the connection env for
   *  every request in this collection (collection wins on key collisions). */
  env?: Record<string, string>;
  /** Headers applied to every request in this collection. A request's own
   *  header with the same name overrides the inherited one. */
  headers?: Record<string, string>;
  /** Default auth for requests in this collection whose own auth is
   *  `none`/unset (inherit from parent). */
  auth?: Auth;
  /** Pre-request script run before every request in this collection (before the
   *  request's own pre-script). */
  preScript?: string;
  /** Test script run after every request in this collection (after the
   *  request's own test script). */
  postScript?: string;
  /** Free-text (Markdown) documentation for this collection. */
  description?: string;
  /** Set when this collection's file lives outside the connection's workspace
   *  dir, at a user-chosen path on disk (linked, not copied in). Never
   *  serialized into the collection's own JSON — it's registry metadata,
   *  tracked in {@link EXTERNAL_LINKS_FILE}. `missing` means the path could
   *  not be read on last load (moved/deleted/unmounted); the collection is
   *  shown as an empty placeholder and is never overwritten until relinked. */
  external?: { path: string; missing?: boolean };
}

export interface CollectionsFile {
  version: 1;
  collections: HttpCollection[];
}

/** A named set of `{{VAR}}` values. The active environment supplies the base
 *  variable map for every request; a collection's own `env` overrides it. */
export interface HttpEnvironment {
  id: string;
  name: string;
  variables: Record<string, string>;
}

/** All environments for a connection plus the id of the active one (`null` =
 *  none selected). Persisted as a single `environments.json` at the workspace
 *  root, a sibling of the collections tree. */
export interface EnvironmentsFile {
  version: 1;
  environments: HttpEnvironment[];
  activeId: string | null;
}

export function defaultEnvironmentsFile(): EnvironmentsFile {
  return { version: 1, environments: [], activeId: null };
}

export function newEnvironment(name = "New environment"): HttpEnvironment {
  return { id: genId(), name, variables: {} };
}

export const HTTP_METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] as const;

export function defaultCollectionsFile(): CollectionsFile {
  return {
    version: 1,
    collections: [
      {
        id: genId(),
        name: "Default",
        folders: [],
        requests: [],
      },
    ],
  };
}

export function newRequest(name = "New request"): HttpRequestItem {
  return {
    id: genId(),
    name,
    method: "GET",
    url: "",
    headers: {},
    body: "",
    body_kind: "none",
    auth: defaultAuth(),
    inheritHeaders: true,
  };
}

// --- On-disk collection v2.1 serialization ----------------------------------
//
// The curl workspace persists as a set of collection-v2.1 JSON files directly
// under its connection profile's workspace dir (`workspace/<connId>/`), one file
// per collection and one per environment:
//   <collectionId>.postman_collection.json   Collection v2.1 (requests
//                                             embedded in the nested `item[]`)
//   <envId>.postman_environment.json          environment
//   .polyclient-env-meta.json                         sidecar: which env is active
// The in-memory model (HttpCollection/HttpFolder/HttpRequestItem/...) stays
// unchanged; the pure functions below map to/from the wire shape, and
// loadCurlFiles/saveCurlFiles do the actual disk IO over the generic
// per-connection path primitives in store.ts (the host knows nothing about this
// layout). Fields the v2.1 format has no home for (collection-level default
// headers, per-request header inheritance, authored order) are preserved in a
// namespaced `_polyclient` extension object, which other tools ignore on import.

/** One stored curl file: path relative to the connection root + its contents. */
export interface CurlFile {
  path: string;
  content: string;
}

/** Filename suffixes / sidecar for the collection-format workspace files. */
export const COLLECTION_SUFFIX = ".postman_collection.json";
export const ENV_SUFFIX = ".postman_environment.json";
export const ENV_META_FILE = ".polyclient-env-meta.json";

/** Temp directory (under the connection root) holding one autosaved draft file
 *  per open request with unsaved edits, plus the open-tabs session sidecar.
 *  These are polyclient-private: the collection loaders/pruners key off the suffixes
 *  above and skip directories, so drafts never round-trip into a collection. */
export const DRAFTS_DIR = ".polyclient-drafts";
export const SESSION_FILE = ".polyclient-curl-session.json";

/** Sidecar listing which collections are linked to a file outside the
 *  workspace dir, and where. See {@link HttpCollection.external}. */
export const EXTERNAL_LINKS_FILE = ".polyclient-external-collections.json";

export interface ExternalLink {
  id: string;
  path: string;
}

interface ExternalLinksFile {
  version: 1;
  links: ExternalLink[];
}

/** The collection v2.1 schema URL, written into every collection file. */
const SCHEMA_URL = "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

/** Namespace for fields the v2.1 format has no home for. Other tools ignore
 *  unknown properties on import, so this round-trips losslessly for us while
 *  keeping the files importable elsewhere. */
const POLYCLIENT_EXT = "_polyclient" as const;

// --- Collection v2.1 wire shapes (only the subset we read/write) ------------

interface WireVariable {
  key: string;
  value?: string;
  disabled?: boolean;
}

interface WireHeader {
  key: string;
  value: string;
  disabled?: boolean;
}

interface WireQueryParam {
  key: string;
  value?: string;
  disabled?: boolean;
}

interface WireUrl {
  raw?: string;
  protocol?: string;
  host?: string | string[];
  path?: string | string[];
  query?: WireQueryParam[];
}

interface WireFormData {
  key: string;
  type?: "text" | "file";
  value?: string;
  /** File-part source path (the format puts the path here, not in `value`). May
   *  be a string, array, or null in foreign files. */
  src?: string | string[] | null;
  fileName?: string;
  contentType?: string;
  disabled?: boolean;
}

interface WireUrlEncoded {
  key: string;
  value?: string;
  disabled?: boolean;
}

interface WireBody {
  mode?: "raw" | "urlencoded" | "formdata" | "file" | "graphql";
  raw?: string;
  urlencoded?: WireUrlEncoded[];
  formdata?: WireFormData[];
  options?: { raw?: { language?: string } };
}

interface WireAuthAttr {
  key: string;
  value?: string;
  type?: string;
}

interface WireAuth {
  type: string;
  bearer?: WireAuthAttr[];
  basic?: WireAuthAttr[];
  apikey?: WireAuthAttr[];
}

interface WireScript {
  type?: string;
  /** One line per element, or a single string (both are schema-legal). */
  exec?: string[] | string;
}

interface WireEvent {
  listen: string;
  script?: WireScript;
}

interface WireRequest {
  method: string;
  header?: WireHeader[];
  url?: string | WireUrl;
  body?: WireBody;
  auth?: WireAuth;
  description?: string;
}

interface PolyclientItemExt {
  inheritHeaders?: boolean;
}

interface WireRequestItem {
  name: string;
  _postman_id?: string;
  event?: WireEvent[];
  request: WireRequest;
  [POLYCLIENT_EXT]?: PolyclientItemExt;
}

interface WireFolderItem {
  name: string;
  _postman_id?: string;
  description?: string;
  item: WireItem[];
}

type WireItem = WireFolderItem | WireRequestItem;

interface PolyclientCollectionExt {
  headers?: Record<string, string>;
  order?: number;
}

interface WireCollection {
  info: { name?: string; _postman_id?: string; description?: string; schema?: string };
  variable?: WireVariable[];
  auth?: WireAuth;
  event?: WireEvent[];
  item?: WireItem[];
  [POLYCLIENT_EXT]?: PolyclientCollectionExt;
}

interface WireEnvValue {
  key: string;
  value?: string;
  enabled?: boolean;
  type?: string;
}

interface WireEnvironment {
  id?: string;
  name?: string;
  values?: WireEnvValue[];
  _postman_variable_scope?: string;
  [POLYCLIENT_EXT]?: { order?: number };
}

// --- Small mapping helpers --------------------------------------------------

/** Value of a wire auth attribute by key ("token"/"username"/...). */
function wireAuthValue(arr: WireAuthAttr[] | undefined, key: string): string {
  return (arr?.find((a) => a.key === key)?.value ?? "").toString();
}

/** in-memory Auth -> wire auth. `inherit` (and a collection's `none`/unset)
 *  map to *absence* of the field, which is the format's "inherit from parent". */
function authToWire(auth: Auth | undefined): WireAuth | undefined {
  switch (auth?.kind) {
    case "bearer":
      return {
        type: "bearer",
        bearer: [{ key: "token", value: auth.token ?? "", type: "string" }],
      };
    case "basic":
      return {
        type: "basic",
        basic: [
          { key: "username", value: auth.username ?? "", type: "string" },
          { key: "password", value: auth.password ?? "", type: "string" },
        ],
      };
    case "apikey":
      return {
        type: "apikey",
        apikey: [
          { key: "key", value: auth.key ?? "", type: "string" },
          { key: "value", value: auth.value ?? "", type: "string" },
          { key: "in", value: auth.in ?? "header", type: "string" },
        ],
      };
    case "none":
      return { type: "noauth" };
    case "inherit":
    default:
      return undefined;
  }
}

/** Wire auth -> in-memory Auth. Absent auth is `inherit`; unsupported types
 *  (oauth2/digest/awsv4/...) degrade to `none` (credentials dropped). */
function wireToAuth(wire: WireAuth | undefined): Auth {
  if (!wire) return { kind: "inherit", in: "header" };
  switch (wire.type) {
    case "noauth":
      return { kind: "none", in: "header" };
    case "bearer":
      return { kind: "bearer", token: wireAuthValue(wire.bearer, "token"), in: "header" };
    case "basic":
      return {
        kind: "basic",
        username: wireAuthValue(wire.basic, "username"),
        password: wireAuthValue(wire.basic, "password"),
        in: "header",
      };
    case "apikey":
      return {
        kind: "apikey",
        key: wireAuthValue(wire.apikey, "key"),
        value: wireAuthValue(wire.apikey, "value"),
        in: wireAuthValue(wire.apikey, "in") === "query" ? "query" : "header",
      };
    default:
      return { kind: "none", in: "header" };
  }
}

/** Non-empty pre/post scripts -> wire `event[]` (one line per exec element). */
function scriptsToEvents(preScript?: string, postScript?: string): WireEvent[] {
  const events: WireEvent[] = [];
  if (preScript && preScript.length) {
    events.push({
      listen: "prerequest",
      script: { type: "text/javascript", exec: preScript.split("\n") },
    });
  }
  if (postScript && postScript.length) {
    events.push({
      listen: "test",
      script: { type: "text/javascript", exec: postScript.split("\n") },
    });
  }
  return events;
}

/** Rewrite the imported `pm.` script namespace to the sandbox's `client.` so
 *  scripts from imported collections run against our runner's API
 *  (`pm.test` -> `client.test`, `pm.environment.set` -> `client.environment.set`,
 *  etc.). Matches `pm` only as a standalone identifier — the preceding char must
 *  be the string start or a non-identifier that isn't `.` — so it never touches
 *  a property access like `foo.pm.bar` or a longer name like `warmup`. Applied
 *  only on collection import (see {@link collectionFromJson}), never on the
 *  internal wire-format round-trip. */
function rewriteScriptNamespace(script: string): string {
  return script.replace(/(^|[^\w$.])pm\./g, "$1client.");
}

/** Read one script string out of an `event[]` by its `listen` name. */
function eventScript(events: WireEvent[] | undefined, listen: string): string | undefined {
  const exec = events?.find((e) => e.listen === listen)?.script?.exec;
  const text = Array.isArray(exec) ? exec.join("\n") : (exec ?? "");
  return text.trim() ? text : undefined;
}

/** in-memory body (`body` + `body_kind` + `parts`) -> wire `request.body`.
 *  Returns undefined for `none` (the format omits the field). */
function bodyToWire(req: HttpRequestItem): WireBody | undefined {
  switch (req.body_kind) {
    case "json":
      return { mode: "raw", raw: req.body ?? "", options: { raw: { language: "json" } } };
    case "text":
      return { mode: "raw", raw: req.body ?? "", options: { raw: { language: "text" } } };
    case "form": {
      // Reuse the form split/collapse logic so the verbatim `a=b&c=d` string
      // maps to the format's structured pairs (dropping the trailing blank row).
      const urlencoded = formToRows(req.body ?? "")
        .filter((r) => r.key.trim())
        .map((r) => ({ key: r.key, value: r.value }));
      return { mode: "urlencoded", urlencoded };
    }
    case "multipart":
      return { mode: "formdata", formdata: (req.parts ?? []).map(partToFormData) };
    case "none":
    default:
      return undefined;
  }
}

/** Wire `request.body` -> the in-memory body fields. */
function wireToBody(
  body: WireBody | undefined,
): Pick<HttpRequestItem, "body" | "body_kind" | "parts"> {
  switch (body?.mode) {
    case "raw": {
      const raw = body.raw ?? "";
      const lang = body.options?.raw?.language;
      const isJson = lang === "json" || (!lang && isProbablyJson(raw));
      return { body: raw, body_kind: isJson ? "json" : "text" };
    }
    case "urlencoded": {
      const bodyStr = (body.urlencoded ?? [])
        .filter((p) => p.disabled !== true && p.key.trim())
        .map((p) => `${p.key}=${p.value ?? ""}`)
        .join("&");
      return { body: bodyStr, body_kind: "form" };
    }
    case "formdata":
      return { body: "", body_kind: "multipart", parts: (body.formdata ?? []).map(formDataToPart) };
    // "file"/"graphql"/"none"/absent have no in-memory equivalent -> none.
    default:
      return { body: "", body_kind: "none" };
  }
}

/** Best-effort JSON detection for a raw body that carries no `language` hint. */
function isProbablyJson(raw: string): boolean {
  const t = raw.trim();
  if (!t || (t[0] !== "{" && t[0] !== "[")) return false;
  try {
    JSON.parse(t);
    return true;
  } catch {
    return false;
  }
}

/** MultipartPart -> wire formdata entry. Our model always keeps the file path
 *  in `value`; the format keeps it in `src` for file parts. */
function partToFormData(p: MultipartPart): WireFormData {
  const out: WireFormData = { key: p.name, type: p.kind };
  if (p.kind === "file") out.src = p.value;
  else out.value = p.value;
  if (p.filename) out.fileName = p.filename;
  if (p.content_type) out.contentType = p.content_type;
  return out;
}

/** Wire formdata entry -> MultipartPart. Coerces a string[]/null `src`. */
function formDataToPart(fd: WireFormData): MultipartPart {
  const kind = fd.type === "file" ? "file" : "text";
  const src = Array.isArray(fd.src) ? (fd.src[0] ?? "") : (fd.src ?? "");
  const part: MultipartPart = {
    name: fd.key,
    kind,
    value: kind === "file" ? src : (fd.value ?? ""),
  };
  if (fd.fileName) part.filename = fd.fileName;
  if (fd.contentType) part.content_type = fd.contentType;
  return part;
}

/** Resolve a wire `url` (string or object) to our single raw-string form. */
function wireUrlToString(url: string | WireUrl | undefined): string {
  if (url == null) return "";
  if (typeof url === "string") return url;
  if (url.raw != null) return url.raw;
  // Reconstruct a best-effort raw URL from a foreign file that omitted `raw`.
  const host = Array.isArray(url.host) ? url.host.join(".") : (url.host ?? "");
  const path = Array.isArray(url.path) ? url.path.join("/") : (url.path ?? "");
  const scheme = url.protocol ? `${url.protocol}://` : "";
  const base = `${scheme}${host}${path ? "/" + path.replace(/^\//, "") : ""}`;
  const query = (url.query ?? [])
    .filter((q) => q.disabled !== true && q.key.trim())
    .map((q) => `${q.key}=${q.value ?? ""}`)
    .join("&");
  return query ? `${base}?${query}` : base;
}

// --- Collection tree mappers ------------------------------------------------

function requestToWireItem(r: HttpRequestItem): WireRequestItem {
  const request: WireRequest = {
    method: r.method,
    header: Object.entries(r.headers).map(([key, value]) => ({ key, value })),
    url: { raw: r.url },
  };
  const body = bodyToWire(r);
  if (body) request.body = body;
  const auth = authToWire(r.auth ?? { kind: "inherit" });
  if (auth) request.auth = auth;
  if (r.description) request.description = r.description;

  const item: WireRequestItem = { name: r.name, _postman_id: r.id, request };
  const events = scriptsToEvents(r.preScript, r.postScript);
  if (events.length) item.event = events;
  if (r.inheritHeaders === false) item[POLYCLIENT_EXT] = { inheritHeaders: false };
  return item;
}

function folderToWireItem(f: HttpFolder): WireFolderItem {
  return {
    name: f.name,
    _postman_id: f.id,
    ...(f.description ? { description: f.description } : {}),
    // Folders first, then requests (the relative order across the two arrays is
    // not representable in the in-memory model).
    item: [...f.folders.map(folderToWireItem), ...f.requests.map(requestToWireItem)],
  };
}

function collectionToWire(c: HttpCollection, order: number): WireCollection {
  const wire: WireCollection = {
    info: {
      name: c.name,
      _postman_id: c.id,
      schema: SCHEMA_URL,
      ...(c.description ? { description: c.description } : {}),
    },
    item: [...c.folders.map(folderToWireItem), ...c.requests.map(requestToWireItem)],
  };
  const variable = Object.entries(c.env ?? {}).map(([key, value]) => ({ key, value }));
  if (variable.length) wire.variable = variable;
  const auth = authToWire(c.auth ?? { kind: "none" });
  if (auth) wire.auth = auth;
  const events = scriptsToEvents(c.preScript, c.postScript);
  if (events.length) wire.event = events;

  const ext: PolyclientCollectionExt = { order };
  if (c.headers && Object.keys(c.headers).length) ext.headers = c.headers;
  wire[POLYCLIENT_EXT] = ext;
  return wire;
}

function wireItemToRequest(item: WireRequestItem): HttpRequestItem {
  const req = item.request;
  const headers: Record<string, string> = {};
  for (const h of req.header ?? []) {
    if (h.disabled === true || !h.key.trim()) continue;
    headers[h.key] = h.value ?? "";
  }
  const out: HttpRequestItem = {
    id: item._postman_id || genId(),
    name: item.name || "New request",
    method: req.method || "GET",
    url: wireUrlToString(req.url),
    headers,
    auth: wireToAuth(req.auth),
    inheritHeaders: item[POLYCLIENT_EXT]?.inheritHeaders ?? true,
    ...wireToBody(req.body),
  };
  const pre = eventScript(item.event, "prerequest");
  const post = eventScript(item.event, "test");
  if (pre) out.preScript = pre;
  if (post) out.postScript = post;
  if (req.description) out.description = req.description;
  return out;
}

function wireItemToFolder(item: WireFolderItem): HttpFolder {
  const { folders, requests } = splitItems(item.item ?? []);
  const out: HttpFolder = {
    id: item._postman_id || genId(),
    name: item.name || "Folder",
    folders,
    requests,
  };
  if (item.description) out.description = item.description;
  return out;
}

/** Partition a wire `item[]` into our two arrays by discriminator: an item
 *  with `item` is a folder, an item with `request` is a request. */
function splitItems(items: WireItem[]): { folders: HttpFolder[]; requests: HttpRequestItem[] } {
  const folders: HttpFolder[] = [];
  const requests: HttpRequestItem[] = [];
  for (const it of items) {
    if ("item" in it && Array.isArray(it.item)) folders.push(wireItemToFolder(it));
    else if ("request" in it && it.request) requests.push(wireItemToRequest(it));
  }
  return { folders, requests };
}

function wireToCollection(wire: WireCollection): HttpCollection & { _order?: number } {
  const { folders, requests } = splitItems(wire.item ?? []);
  const env: Record<string, string> = {};
  for (const v of wire.variable ?? []) {
    if (v.disabled === true) continue;
    env[v.key] = v.value ?? "";
  }
  const c: HttpCollection & { _order?: number } = {
    id: wire.info?._postman_id || genId(),
    name: wire.info?.name || "Untitled",
    folders,
    requests,
    auth: wireToAuth(wire.auth),
    _order: wire[POLYCLIENT_EXT]?.order,
  };
  if (Object.keys(env).length) c.env = env;
  const extHeaders = wire[POLYCLIENT_EXT]?.headers;
  if (extHeaders && Object.keys(extHeaders).length) c.headers = extHeaders;
  const pre = eventScript(wire.event, "prerequest");
  const post = eventScript(wire.event, "test");
  if (pre) c.preScript = pre;
  if (post) c.postScript = post;
  if (wire.info?.description) c.description = wire.info.description;
  return c;
}

/** Sort by the `_order` we stamped on write (stable; absent -> 0), then drop it. */
function sortAndStripOrder<T extends { _order?: number }>(items: T[]): Omit<T, "_order">[] {
  return items
    .map((it, i) => ({ it, key: it._order ?? i }))
    .sort((a, b) => a.key - b.key)
    .map(({ it }) => {
      const { _order, ...rest } = it;
      void _order;
      return rest;
    });
}

/** Serialize the in-memory tree into the on-disk file list: one collection
 *  file per collection, keyed by collection id. */
export function collectionsToFiles(data: CollectionsFile): CurlFile[] {
  return data.collections.map((c, i) => ({
    path: `${c.id}${COLLECTION_SUFFIX}`,
    content: JSON.stringify(collectionToWire(c, i), null, 2),
  }));
}

/** Rebuild the in-memory tree from the on-disk file list: parse every
 *  collection file, map each to a collection, and order by the stamped
 *  `_polyclient.order`. Falls back to a default file when none are present. */
export function filesToCollections(files: CurlFile[]): CollectionsFile {
  const cols = files
    .filter((f) => f.path.endsWith(COLLECTION_SUFFIX))
    .map((f) => {
      try {
        return wireToCollection(JSON.parse(f.content) as WireCollection);
      } catch {
        return null;
      }
    })
    .filter((c): c is HttpCollection & { _order?: number } => !!c);
  if (cols.length === 0) return defaultCollectionsFile();
  return { version: 1, collections: sortAndStripOrder(cols) };
}

// --- Single-collection import / export (v2.1) -------------------------------
//
// The on-disk workspace format is already v2.1, so importing/exporting a
// single collection to/from a user-chosen file reuses the same mappers. Export
// serializes one collection; import parses one collection file and hands
// back an in-memory collection to splice into the tree.

/** Serialize one collection to a v2.1 collection JSON string, suitable for
 *  saving to a file and re-importing here or into other v2.1-compatible tools. */
export function collectionToJson(c: HttpCollection): string {
  return JSON.stringify(collectionToWire(c, 0), null, 2);
}

/** A filesystem-safe default filename for exporting a collection, e.g.
 *  `My API.postman_collection.json`. */
export function collectionFileName(c: HttpCollection): string {
  const base = c.name
    .trim()
    .replace(/[^\w.-]+/g, "_")
    .replace(/^_+|_+$/g, "");
  return `${base || "collection"}${COLLECTION_SUFFIX}`;
}

/** Parse+validate a v2.1 collection JSON string into an in-memory collection,
 *  with no id regeneration or script rewriting. Throws when the text is not
 *  JSON or does not look like a collection. Shared by {@link collectionFromJson}
 *  (import: a copy) and {@link parseLinkedCollectionFile} (a link: same file
 *  stays the source of truth, so ids must round-trip via `_postman_id`). */
function parseCollectionJson(json: string): HttpCollection {
  let wire: WireCollection;
  try {
    wire = JSON.parse(json) as WireCollection;
  } catch {
    throw new Error("File is not valid JSON");
  }
  if (!wire || typeof wire !== "object" || (!wire.info && !Array.isArray(wire.item))) {
    throw new Error("File is not a valid collection");
  }
  const { _order, ...c } = wireToCollection(wire);
  void _order;
  return c;
}

/** Parse a v2.1 collection JSON string into an in-memory collection.
 *  All ids are regenerated and every script's `pm.` namespace is rewritten to
 *  our `client.` API (see {@link prepareImported}). Throws when the text is not
 *  JSON or does not look like a collection. */
export function collectionFromJson(json: string): HttpCollection {
  return prepareImported(parseCollectionJson(json), { regenerateIds: true });
}

/** Parse a v2.1 collection JSON string read from a file the user is linking
 *  (not importing/copying): ids are kept as-authored (so the file's own
 *  `_postman_id`s round-trip on the next save) but `pm.` scripts are still
 *  rewritten to our `client.` API, since a foreign Postman export uses `pm.`. */
export function parseLinkedCollectionFile(json: string): HttpCollection {
  return prepareImported(parseCollectionJson(json), { regenerateIds: false });
}

/** Post-process a freshly-parsed collection: rewrite each script's `pm.`
 *  namespace to our `client.` API, and — for an import (`regenerateIds: true`)
 *  only — regenerate every id (collection, every folder, every request) so it
 *  can never collide with one already open — importing the same file twice
 *  yields two independent collections and per-request draft files (keyed by
 *  request id) stay separate. A linked collection keeps its ids so the file
 *  it was read from round-trips on save. */
function prepareImported(
  c: HttpCollection,
  { regenerateIds }: { regenerateIds: boolean },
): HttpCollection {
  const convertReq = (r: HttpRequestItem): HttpRequestItem => ({
    ...r,
    ...(regenerateIds ? { id: genId() } : {}),
    ...(r.preScript ? { preScript: rewriteScriptNamespace(r.preScript) } : {}),
    ...(r.postScript ? { postScript: rewriteScriptNamespace(r.postScript) } : {}),
  });
  const walkFolder = (f: HttpFolder): HttpFolder => ({
    ...f,
    ...(regenerateIds ? { id: genId() } : {}),
    folders: f.folders.map(walkFolder),
    requests: f.requests.map(convertReq),
  });
  return {
    ...c,
    ...(regenerateIds ? { id: genId() } : {}),
    folders: c.folders.map(walkFolder),
    requests: c.requests.map(convertReq),
    ...(c.preScript ? { preScript: rewriteScriptNamespace(c.preScript) } : {}),
    ...(c.postScript ? { postScript: rewriteScriptNamespace(c.postScript) } : {}),
  };
}

// --- Disk IO over the generic workspace primitives --------------------------

/** Load every collection file for a profile as a flat `{path, content}` list.
 *  Empty when nothing is saved yet. Feed the result to {@link filesToCollections}. */
export async function loadCurlFiles(savedId: string): Promise<CurlFile[]> {
  const entries = await listWorkspaceDir(savedId, "");
  const files: CurlFile[] = [];
  for (const e of entries) {
    if (e.isDir || !e.name.endsWith(COLLECTION_SUFFIX)) continue;
    const content = await readWorkspaceFile(savedId, e.name);
    if (content !== null) files.push({ path: e.name, content });
  }
  return files;
}

/** Persist the collection file set (from {@link collectionsToFiles}) for a
 *  profile: write every desired file, then prune stale collection files
 *  no longer wanted so deletions are reflected on disk. Restricting the
 *  prune to that suffix inherently protects the environment files and sidecar. */
export async function saveCurlFiles(savedId: string, files: CurlFile[]): Promise<void> {
  const wanted = new Set(files.map((f) => f.path));
  for (const f of files) {
    await writeWorkspaceFileAt(savedId, f.path, f.content);
  }
  const entries = await listWorkspaceDir(savedId, "");
  for (const e of entries) {
    if (!e.isDir && e.name.endsWith(COLLECTION_SUFFIX) && !wanted.has(e.name)) {
      await deleteWorkspacePath(savedId, e.name);
    }
  }
}

// --- External (linked) collection files --------------------------------------
//
// A collection can instead be backed by a file at an arbitrary path outside
// the workspace dir (chosen via a directory or file picker), so it stays the
// source of truth and edits write straight back to it. Which collection ids
// are linked to which paths is tracked in EXTERNAL_LINKS_FILE, a sidecar in
// the same workspace dir as the collection files; the linked files themselves
// are read/written through the unsandboxed fs plugin, not the workspace
// primitives above.

/** Load the id -> path registry of linked external collections. `[]` when
 *  none are linked or the sidecar is missing/corrupt. */
export async function loadExternalLinks(savedId: string): Promise<ExternalLink[]> {
  const content = await readWorkspaceFile(savedId, EXTERNAL_LINKS_FILE);
  if (content === null) return [];
  try {
    const parsed = JSON.parse(content) as ExternalLinksFile;
    return Array.isArray(parsed.links) ? parsed.links : [];
  } catch {
    return [];
  }
}

/** Persist the id -> path registry of linked external collections. */
export function saveExternalLinks(savedId: string, links: ExternalLink[]): Promise<void> {
  const data: ExternalLinksFile = { version: 1, links };
  return writeWorkspaceFileAt(savedId, EXTERNAL_LINKS_FILE, JSON.stringify(data, null, 2));
}

/** Read and parse a linked collection file from its external path. Throws if
 *  the path can't be read or doesn't look like a collection — callers should
 *  fall back to a `missing` placeholder rather than let this fail the load. */
export async function readExternalCollectionFile(path: string): Promise<HttpCollection> {
  const text = await readTextFile(path);
  return parseLinkedCollectionFile(text);
}

/** Write a linked collection back to its external path. */
export function writeExternalCollectionFile(
  path: string,
  collection: HttpCollection,
): Promise<void> {
  return writeTextFile(path, collectionToJson(collection));
}

/** in-memory environment -> wire environment file shape. */
function envToWire(env: HttpEnvironment, order: number): WireEnvironment {
  return {
    id: env.id,
    name: env.name,
    values: Object.entries(env.variables).map(([key, value]) => ({
      key,
      value,
      enabled: true,
      type: "default",
    })),
    _postman_variable_scope: "environment",
    [POLYCLIENT_EXT]: { order },
  };
}

/** Wire environment file -> in-memory environment (dropping disabled values). */
function wireToEnv(wire: WireEnvironment): HttpEnvironment & { _order?: number } {
  const variables: Record<string, string> = {};
  for (const v of wire.values ?? []) {
    if (v.enabled === false || !v.key) continue;
    variables[v.key] = v.value ?? "";
  }
  return {
    id: wire.id || genId(),
    name: wire.name || "Environment",
    variables,
    _order: wire[POLYCLIENT_EXT]?.order,
  };
}

/** Load all environments for a profile, plus the active selection from the
 *  sidecar. Falls back to an empty set when nothing is saved yet. */
export async function loadEnvironments(savedId: string): Promise<EnvironmentsFile> {
  const entries = await listWorkspaceDir(savedId, "");
  const envs: (HttpEnvironment & { _order?: number })[] = [];
  for (const e of entries) {
    if (e.isDir || !e.name.endsWith(ENV_SUFFIX)) continue;
    const content = await readWorkspaceFile(savedId, e.name);
    if (content === null) continue;
    try {
      envs.push(wireToEnv(JSON.parse(content) as WireEnvironment));
    } catch {
      // Skip an unparseable environment file rather than failing the whole load.
    }
  }
  const environments = sortAndStripOrder(envs);

  let activeId: string | null = null;
  const meta = await readWorkspaceFile(savedId, ENV_META_FILE);
  if (meta !== null) {
    try {
      activeId = (JSON.parse(meta) as { activeId?: string | null }).activeId ?? null;
    } catch {
      // Ignore a corrupt sidecar; treat as no active environment.
    }
  }
  // Guard against a dangling activeId that no longer names an existing env.
  if (activeId && !environments.some((e) => e.id === activeId)) activeId = null;

  if (environments.length === 0 && activeId === null) return defaultEnvironmentsFile();
  return { version: 1, environments, activeId };
}

/** Persist all environments for a profile: one environment file each,
 *  plus the active-selection sidecar. Prunes stale environment files. */
export async function saveEnvironments(savedId: string, data: EnvironmentsFile): Promise<void> {
  const wanted = new Set<string>();
  for (let i = 0; i < data.environments.length; i++) {
    const env = data.environments[i];
    const path = `${env.id}${ENV_SUFFIX}`;
    wanted.add(path);
    await writeWorkspaceFileAt(savedId, path, JSON.stringify(envToWire(env, i), null, 2));
  }
  await writeWorkspaceFileAt(
    savedId,
    ENV_META_FILE,
    JSON.stringify({ version: 1, activeId: data.activeId ?? null }, null, 2),
  );
  const entries = await listWorkspaceDir(savedId, "");
  for (const e of entries) {
    if (!e.isDir && e.name.endsWith(ENV_SUFFIX) && !wanted.has(e.name)) {
      await deleteWorkspacePath(savedId, e.name);
    }
  }
}

// --- Drafts + open-tabs session (restart-survivable, per-request) -----------

/** An autosaved draft of one open request's unsaved edits. `key` is the tab
 *  key it belongs to (`collectionId:folderId:requestId` for a collection-backed
 *  request, or `tmp␟<id>` for a scratch request). `kind` distinguishes a
 *  scratch request (not yet in any collection) from an edited saved request. */
export interface DraftEntry {
  version: 1;
  key: string;
  kind: "scratch" | "collection";
  request: HttpRequestItem;
}

/** The open-tabs session for a profile: which request/collection/folder/env
 *  tabs are open, which is active, and the expanded tree state. Persisted so
 *  open requests survive an app restart. */
export interface CurlSession {
  version: 1;
  openTabs: string[];
  selectedId: string | null;
  expanded: Record<string, boolean>;
}

/** Load every draft file for a profile. Skips unparseable files rather than
 *  failing the whole load (mirroring {@link filesToCollections}). */
export async function loadDrafts(savedId: string): Promise<DraftEntry[]> {
  const entries = await listWorkspaceDir(savedId, DRAFTS_DIR);
  const drafts: DraftEntry[] = [];
  for (const e of entries) {
    if (e.isDir || !e.name.endsWith(".json")) continue;
    const content = await readWorkspaceFile(savedId, `${DRAFTS_DIR}/${e.name}`);
    if (content === null) continue;
    try {
      const d = JSON.parse(content) as DraftEntry;
      if (d && d.key && d.request?.id) drafts.push(d);
    } catch {
      // Ignore a corrupt draft file.
    }
  }
  return drafts;
}

/** Write one draft file (named by its request id, which is unique per profile
 *  and filesystem-safe). */
export function saveDraft(savedId: string, entry: DraftEntry): Promise<void> {
  return writeWorkspaceFileAt(
    savedId,
    `${DRAFTS_DIR}/${entry.request.id}.json`,
    JSON.stringify(entry, null, 2),
  );
}

/** Delete a draft file by its request id (missing = success). */
export function deleteDraft(savedId: string, requestId: string): Promise<void> {
  return deleteWorkspacePath(savedId, `${DRAFTS_DIR}/${requestId}.json`);
}

/** Load the open-tabs session, or null when none is saved / it is corrupt. */
export async function loadCurlSession(savedId: string): Promise<CurlSession | null> {
  const content = await readWorkspaceFile(savedId, SESSION_FILE);
  if (content === null) return null;
  try {
    const s = JSON.parse(content) as CurlSession;
    return {
      version: 1,
      openTabs: Array.isArray(s.openTabs) ? s.openTabs : [],
      selectedId: s.selectedId ?? null,
      expanded: s.expanded ?? {},
    };
  } catch {
    return null;
  }
}

/** Persist the open-tabs session. */
export function saveCurlSession(savedId: string, session: CurlSession): Promise<void> {
  return writeWorkspaceFileAt(savedId, SESSION_FILE, JSON.stringify(session, null, 2));
}

export function methodColor(method: string): string {
  switch (method.toUpperCase()) {
    case "GET":
      return "get";
    case "POST":
      return "post";
    case "PUT":
      return "put";
    case "PATCH":
      return "patch";
    case "DELETE":
      return "delete";
    case "HEAD":
    case "OPTIONS":
      return "info";
    default:
      return "other";
  }
}

/** An editable key/value row used for headers and query params. */
export interface KvRow {
  id: string;
  enabled: boolean;
  key: string;
  value: string;
}

export function newKvRow(key = "", value = "", enabled = true): KvRow {
  return { id: genId(), enabled, key, value };
}

/** The User-Agent polyclient sends, embedding the running plugin's version (which is
 *  the plugin's `CARGO_PKG_VERSION`, so it matches the backend client default). */
export function userAgent(version: string): string {
  return `polyclient/${version || "0"} Http-Client (plugin)`;
}

/** Headers polyclient adds to every request automatically. They are
 *  shown read-only in the Headers tab and sent unless the user (or the owning
 *  collection) sets a header with the same name, which overrides them. */
export function autoHeaders(version: string): Record<string, string> {
  return { "User-Agent": userAgent(version) };
}

/** Read-only display rows for {@link autoHeaders}. Any auto header whose name
 *  the caller's own (enabled) header keys override — case-insensitively — is
 *  marked `enabled: false` so the UI can show it as inactive/struck-through. */
export function autoHeaderRows(overrideKeys: string[], version: string): KvRow[] {
  const overridden = new Set(overrideKeys.map((k) => k.trim().toLowerCase()).filter(Boolean));
  return Object.entries(autoHeaders(version)).map(([k, v]) => ({
    id: "auto:" + k,
    enabled: !overridden.has(k.toLowerCase()),
    key: k,
    value: v,
  }));
}

/** Build rows from a header map, always leaving one trailing blank row. */
export function headersToRows(headers: Record<string, string>): KvRow[] {
  const rows = Object.entries(headers).map(([k, v]) => newKvRow(k, v));
  rows.push(newKvRow());
  return rows;
}

/** Collapse rows back into a header map (enabled, non-empty keys only). */
export function rowsToHeaders(rows: KvRow[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) {
    const key = r.key.trim();
    if (!r.enabled || !key) continue;
    out[key] = r.value;
  }
  return out;
}

/** Parse a urlencoded form body (`a=1&b=2`) into editable rows. Values are kept
 *  verbatim — no percent-decoding — so `{{VAR}}` placeholders and any
 *  already-encoded text round-trip unchanged through {@link rowsToForm}. Always
 *  leaves one trailing blank row. */
export function formToRows(body: string | null | undefined): KvRow[] {
  const rows: KvRow[] = [];
  for (const pair of (body ?? "").split("&")) {
    if (!pair) continue;
    const eq = pair.indexOf("=");
    const key = eq < 0 ? pair : pair.slice(0, eq);
    const val = eq < 0 ? "" : pair.slice(eq + 1);
    rows.push(newKvRow(key, val));
  }
  rows.push(newKvRow());
  return rows;
}

/** Collapse rows back into a urlencoded form body string (enabled, non-empty
 *  keys only). Written verbatim — the inverse of {@link formToRows}. */
export function rowsToForm(rows: KvRow[]): string {
  return rows
    .filter((r) => r.enabled && r.key.trim())
    .map((r) => `${r.key.trim()}=${r.value}`)
    .join("&");
}

/** Split a URL into its base (scheme://host/path) and query rows. Values are
 *  kept verbatim — no percent-decoding — so `{{VAR}}` placeholders and any
 *  already-encoded text round-trip unchanged through {@link joinUrl}. */
export function splitUrl(url: string): { base: string; params: KvRow[] } {
  const qIdx = url.indexOf("?");
  if (qIdx < 0) return { base: url, params: [newKvRow()] };
  const base = url.slice(0, qIdx);
  const query = url.slice(qIdx + 1);
  const params: KvRow[] = [];
  for (const pair of query.split("&")) {
    if (!pair) continue;
    const eq = pair.indexOf("=");
    const rawKey = eq < 0 ? pair : pair.slice(0, eq);
    const rawVal = eq < 0 ? "" : pair.slice(eq + 1);
    params.push(newKvRow(rawKey, rawVal));
  }
  params.push(newKvRow());
  return { base, params };
}

/** Reassemble a URL from a base and query rows. Keys/values are written
 *  verbatim — the URL input is not percent-encoded, so what the user types is
 *  what gets sent (matching curl / browser address-bar behaviour). */
export function joinUrl(base: string, params: KvRow[]): string {
  const stem = base.split("?")[0];
  const query = params
    .filter((p) => p.enabled && p.key.trim())
    .map((p) => `${p.key.trim()}=${p.value}`)
    .join("&");
  return query ? `${stem}?${query}` : stem;
}

/** UTF-8-safe base64 (btoa alone mishandles non-Latin1 chars). */
export function base64Utf8(s: string): string {
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** One run of literal text, or one `{{NAME}}` placeholder, in a template
 *  string. `name` (trimmed) is only set when `isVar` is true. */
export interface VarToken {
  text: string;
  isVar: boolean;
  name?: string;
}

/** Split a template into literal/placeholder runs for inline UI highlighting.
 *  Unlike {@link interpolate}, this never throws: an unclosed `{{` (as a user
 *  is mid-typing one) is emitted as trailing literal text instead of erroring. */
export function splitVarTokens(template: string): VarToken[] {
  const tokens: VarToken[] = [];
  let rest = template;
  while (rest) {
    const start = rest.indexOf("{{");
    if (start < 0) {
      tokens.push({ text: rest, isVar: false });
      break;
    }
    if (start > 0) tokens.push({ text: rest.slice(0, start), isVar: false });
    rest = rest.slice(start);
    const end = rest.indexOf("}}");
    if (end < 0) {
      tokens.push({ text: rest, isVar: false });
      break;
    }
    tokens.push({ text: rest.slice(0, end + 2), isVar: true, name: rest.slice(2, end).trim() });
    rest = rest.slice(end + 2);
  }
  return tokens;
}

/** Replace `{{NAME}}` placeholders using the environment map. Throws on an
 *  unclosed placeholder, an empty name, or an unknown variable. Mirrors the
 *  behaviour the curlui plugin used to apply server-side. */
export function interpolate(template: string, env: Record<string, string>): string {
  let out = "";
  let rest = template;
  for (let start = rest.indexOf("{{"); start >= 0; start = rest.indexOf("{{")) {
    out += rest.slice(0, start);
    rest = rest.slice(start + 2);
    const end = rest.indexOf("}}");
    if (end < 0) throw new Error("unclosed variable placeholder");
    const name = rest.slice(0, end).trim();
    if (!name) throw new Error("empty variable name");
    if (!(name in env)) throw new Error(`unknown variable: ${name}`);
    out += env[name];
    rest = rest.slice(end + 2);
  }
  return out + rest;
}

/** True for hosts that are almost always plain HTTP local/dev servers
 *  (localhost, loopback, and private-network addresses). */
function isLocalHost(host: string): boolean {
  const h = host.toLowerCase();
  if (h === "localhost" || h === "::1" || h === "0.0.0.0") return true;
  const m = h.match(/^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/);
  if (!m) return false;
  const a = Number(m[1]);
  const b = Number(m[2]);
  return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
}

/** Default a schemeless URL to `https://` so a bare host like `www.google.com`
 *  works like it does in a browser — except localhost/private-network hosts,
 *  which default to `http://` since local dev servers rarely speak TLS.
 *  A leading `scheme://` is left untouched. */
export function normalizeUrl(url: string): string {
  const trimmed = url.trim();
  if (!trimmed || /^[a-zA-Z][a-zA-Z0-9+\-.]*:\/\//.test(trimmed)) return trimmed;
  const hostPart = trimmed.split(/[/?#]/)[0].split(":")[0];
  const scheme = isLocalHost(hostPart) ? "http" : "https";
  return `${scheme}://${trimmed}`;
}

/** Build the concrete request to send: merge the owning collection's env
 *  overrides, headers, and default auth, interpolate `{{VAR}}` placeholders,
 *  normalize the URL, then merge the effective auth into headers (and, for an
 *  API key sent as a query param, the URL). Basic credentials are interpolated
 *  *before* base64 so a `{{USER}}`/`{{PASS}}` resolves correctly. Throws (via
 *  {@link interpolate}) on an unknown/malformed placeholder.
 *
 *  Precedence: request headers/auth override the collection's; the collection's
 *  env overrides the connection env. */
export function buildSendable(
  req: HttpRequestItem,
  env: Record<string, string>,
  collection?: Pick<HttpCollection, "env" | "headers" | "auth">,
  pluginVersion = "",
): HttpRequest {
  // Collection env overrides connection env.
  const effEnv = collection?.env ? { ...env, ...collection.env } : env;

  // Collection headers are defaults (when inheritance is on); request headers
  // (same key, case-insensitive) win.
  const headers: Record<string, string> = {};
  const haveKey = (k: string) =>
    Object.keys(headers).some((h) => h.toLowerCase() === k.toLowerCase());
  if (req.inheritHeaders !== false) {
    for (const [k, v] of Object.entries(collection?.headers ?? {})) {
      if (k.trim()) headers[k] = interpolate(v, effEnv);
    }
  }
  for (const [k, v] of Object.entries(req.headers)) {
    if (!k.trim()) continue;
    // Drop any inherited header with the same name before setting the override.
    for (const h of Object.keys(headers)) {
      if (h.toLowerCase() === k.toLowerCase()) delete headers[h];
    }
    headers[k] = interpolate(v, effEnv);
  }

  let url = normalizeUrl(interpolate(req.url, effEnv));

  // Resolve effective auth: a request that inherits (or has no auth set) uses
  // the collection's auth; an explicit `none` means no auth. The collection's
  // own auth may itself be `inherit`/`none`, which resolves to no auth.
  const reqAuth = req.auth ?? { kind: "inherit" as AuthKind };
  const auth = reqAuth.kind === "inherit" ? collection?.auth : reqAuth;

  switch (auth?.kind) {
    case "bearer":
      if (auth.token?.trim() && !haveKey("Authorization")) {
        headers["Authorization"] = `Bearer ${interpolate(auth.token.trim(), effEnv)}`;
      }
      break;
    case "basic":
      if ((auth.username || auth.password) && !haveKey("Authorization")) {
        const user = interpolate(auth.username ?? "", effEnv);
        const pass = interpolate(auth.password ?? "", effEnv);
        headers["Authorization"] = `Basic ${base64Utf8(`${user}:${pass}`)}`;
      }
      break;
    case "apikey":
      if (auth.key?.trim()) {
        const key = interpolate(auth.key.trim(), effEnv);
        const value = interpolate(auth.value ?? "", effEnv);
        if ((auth.in ?? "header") === "query") {
          const { base, params } = splitUrl(url);
          params.splice(params.length - 1, 0, {
            id: "auth",
            enabled: true,
            key,
            value,
          });
          url = joinUrl(base, params);
        } else if (!haveKey(key)) {
          headers[key] = value;
        }
      }
      break;
  }

  // Add the auto-generated headers (e.g. User-Agent) unless the user or the
  // collection already set one with the same name — a same-name header the user
  // adds overrides the auto value.
  for (const [k, v] of Object.entries(autoHeaders(pluginVersion))) {
    if (!haveKey(k)) headers[k] = v;
  }

  // A body of kind "none" carries no content regardless of any text left in the
  // editor from a previous kind, so don't send it. A "multipart" body carries
  // structured `parts` instead of a body string.
  if (req.body_kind === "multipart") {
    const parts = (req.parts ?? [])
      .filter((p) => p.name.trim() || p.value.trim())
      .map((p) => ({
        name: interpolate(p.name, effEnv),
        kind: p.kind,
        value: interpolate(p.value, effEnv),
        ...(p.filename ? { filename: interpolate(p.filename, effEnv) } : {}),
        ...(p.content_type ? { content_type: interpolate(p.content_type, effEnv) } : {}),
      }));
    return {
      method: req.method,
      url,
      headers,
      body: "",
      body_kind: req.body_kind,
      parts,
    };
  }

  // An `application/x-www-form-urlencoded` body: percent-encode the field pairs
  // (the editor keeps them verbatim) and default the Content-Type header.
  if (req.body_kind === "form") {
    if (!haveKey("Content-Type")) {
      headers["Content-Type"] = "application/x-www-form-urlencoded";
    }
    return {
      method: req.method,
      url,
      headers,
      body: encodeFormBody(req.body ?? "", effEnv),
      body_kind: req.body_kind,
    };
  }

  const body = req.body_kind === "none" ? "" : interpolate(req.body ?? "", effEnv);

  if (req.body_kind === "json" && !haveKey("Content-Type")) {
    headers["Content-Type"] = "application/json";
  } else if (req.body_kind === "text" && !haveKey("Content-Type")) {
    headers["Content-Type"] = "text/plain";
  }

  return {
    method: req.method,
    url,
    headers,
    body,
    body_kind: req.body_kind,
  };
}

/** Percent-encode one `x-www-form-urlencoded` key or value, preserving any
 *  existing `%XX` escapes so an already-encoded value (e.g. from curl import) is
 *  not double-encoded. Iterates by code point so multi-byte chars round-trip.
 *  Mirrors the URL-query `enc` in the curlui plugin. */
export function encodeFormComponent(s: string): string {
  let out = "";
  const chars = Array.from(s);
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i];
    if (
      ch === "%" &&
      i + 2 < chars.length &&
      /^[0-9a-fA-F]$/.test(chars[i + 1]) &&
      /^[0-9a-fA-F]$/.test(chars[i + 2])
    ) {
      out += ch + chars[i + 1] + chars[i + 2];
      i += 2;
      continue;
    }
    out += encodeURIComponent(ch);
  }
  return out;
}

/** Encode a verbatim form body (`a=hello world&b={{V}}`) as
 *  `application/x-www-form-urlencoded`: split on the structural `&`/`=`,
 *  interpolate `{{VAR}}` in each component, then percent-encode each. Splitting
 *  before interpolating keeps a variable whose value contains `&`/`=` from
 *  corrupting the field structure. */
function encodeFormBody(body: string, env: Record<string, string>): string {
  return body
    .split("&")
    .filter((pair) => pair !== "")
    .map((pair) => {
      const eq = pair.indexOf("=");
      if (eq < 0) return encodeFormComponent(interpolate(pair, env));
      const k = encodeFormComponent(interpolate(pair.slice(0, eq), env));
      const v = encodeFormComponent(interpolate(pair.slice(eq + 1), env));
      return `${k}=${v}`;
    })
    .join("&");
}

/** Render a request as a copy-pasteable `curl` command. Mirrors the plugin's
 *  `build_curl_command` output so a copied command matches the "As cURL" view.
 *  Pass the concrete request from {@link buildSendable} so auth and `{{VAR}}`
 *  values are already resolved. */
export function buildCurl(req: HttpRequest): string {
  const parts = [`curl -X ${req.method} '${req.url}'`];
  for (const [k, v] of Object.entries(req.headers)) {
    parts.push(`  -H '${k}: ${v}'`);
  }
  const esc = (s: string) => s.replace(/'/g, "'\\''");
  if (req.body_kind === "multipart") {
    for (const p of req.parts ?? []) {
      if (!p.name.trim()) continue;
      let spec: string;
      if (p.kind === "file") {
        spec = `${p.name}=@${p.value}`;
        if (p.content_type) spec += `;type=${p.content_type}`;
        if (p.filename) spec += `;filename=${p.filename}`;
      } else {
        spec = `${p.name}=${p.value}`;
      }
      parts.push(`  -F '${esc(spec)}'`);
    }
  } else {
    const body = req.body ?? "";
    if (body) {
      parts.push(`  -d '${esc(body)}'`);
    }
  }
  return parts.join(" \\\n");
}
