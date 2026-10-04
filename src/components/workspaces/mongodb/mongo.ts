import { EJSON } from "bson";
import { parseFilter, toJSString } from "mongodb-query-parser";
import type { RunCommandResult } from "../../../api/document.ts";

// Helpers for the MongoDB workspace: aggregation stage metadata (operators +
// Compass-style default snippets), document/index formatting, and the
// `runCommand` command builders that back the three tabs. Kept out of the
// component so the view stays focused on rendering.

/** A single aggregation-pipeline stage in the builder. */
export interface Stage {
  /** Stage operator, e.g. `$match`, `$group`. */
  op: string;
  /** The stage body (shell syntax), e.g. `{ status: "active" }`. */
  body: string;
  /** Disabled stages are kept in the UI but excluded from the run. */
  enabled: boolean;
}

/** A page of documents returned by a find/aggregate command. */
export interface DocsResult {
  documents: unknown[];
  elapsed_ms: number;
}

/** Per-statement outcome shown in the Script tab. On success `result` holds the
 * raw `runCommand` reply (or `value` for a plain expression result); on failure
 * (parse or run error) `error` is set. */
export interface StatementResult {
  index: number;
  source: string;
  collection: string;
  method: string;
  /** The command JSON sent (empty for a non-command statement or an error). */
  command: string;
  result?: RunCommandResult;
  /** A formatted plain value, for statements that returned one without running
   * a command (e.g. `2 + 2`). */
  value?: string;
  error?: string;
}

/** Aggregation stage operators offered in the per-stage dropdown. */
/** Default stage body inserted when an operator is picked, mirroring the
 * starter snippet Compass drops into the editor. The keys are the full set of
 * MongoDB aggregation pipeline stages offered in the per-stage dropdown. */
const STAGE_DEFAULTS: Record<string, string> = {
  $addFields: '{\n  newField: "$expression"\n}',
  $bucket:
    '{\n  groupBy: "$field",\n  boundaries: [0, 10, 20],\n  default: "other",\n  output: { count: { $sum: 1 } }\n}',
  $bucketAuto: '{\n  groupBy: "$field",\n  buckets: 5,\n  output: { count: { $sum: 1 } }\n}',
  $changeStream: '{\n  fullDocument: "updateLookup"\n}',
  $changeStreamSplitLargeEvent: "{}",
  $collStats: "{\n  latencyStats: { histograms: false },\n  storageStats: {},\n  count: {}\n}",
  $count: '"total"',
  $densify: '{\n  field: "field",\n  range: { step: 1, unit: "day", bounds: "full" }\n}',
  $documents: "[\n  { field: 1 }\n]",
  $facet: "{\n  outputField: [\n    // sub-pipeline stages\n  ]\n}",
  $fill: '{\n  sortBy: { field: 1 },\n  output: { field: { method: "linear" } }\n}',
  $geoNear:
    '{\n  near: { type: "Point", coordinates: [0, 0] },\n  distanceField: "distance",\n  spherical: true\n}',
  $graphLookup:
    '{\n  from: "collection",\n  startWith: "$field",\n  connectFromField: "field",\n  connectToField: "field",\n  as: "results"\n}',
  $group: '{\n  _id: "$field",\n  count: { $sum: 1 }\n}',
  $indexStats: "{}",
  $limit: "10",
  $lookup:
    '{\n  from: "otherCollection",\n  localField: "field",\n  foreignField: "_id",\n  as: "results"\n}',
  $match: '{\n  field: "value"\n}',
  $merge:
    '{\n  into: "outputCollection",\n  on: "_id",\n  whenMatched: "merge",\n  whenNotMatched: "insert"\n}',
  $out: '"outputCollection"',
  $project: "{\n  field: 1\n}",
  $redact: '"$$DESCEND"',
  $replaceRoot: '{\n  newRoot: "$field"\n}',
  $replaceWith: '"$field"',
  $sample: "{\n  size: 100\n}",
  $search: '{\n  index: "default",\n  text: { query: "value", path: "field" }\n}',
  $searchMeta: '{\n  index: "default",\n  facet: { facets: {} }\n}',
  $set: '{\n  newField: "$expression"\n}',
  $setWindowFields:
    '{\n  partitionBy: "$field",\n  sortBy: { field: 1 },\n  output: {\n    runningTotal: { $sum: "$value", window: { documents: ["unbounded", "current"] } }\n  }\n}',
  $skip: "0",
  $sort: "{\n  field: -1\n}",
  $sortByCount: '"$field"',
  $unionWith: '{\n  coll: "otherCollection",\n  pipeline: []\n}',
  $unset: '"field"',
  $unwind: '"$arrayField"',
  $vectorSearch:
    '{\n  index: "default",\n  path: "embedding",\n  queryVector: [],\n  numCandidates: 100,\n  limit: 10\n}',
};

/** All aggregation stage operators (the dropdown options), alphabetical. */
export const STAGE_OPS = Object.keys(STAGE_DEFAULTS).sort();

/** The starter snippet for a stage operator (`{}` when none is defined). */
export function stageDefault(op: string): string {
  return STAGE_DEFAULTS[op] ?? "{}";
}

/** A fresh stage, pre-filled with the operator's default snippet. */
export function newStage(op = "$match"): Stage {
  return { op, body: stageDefault(op), enabled: true };
}

/** Initial pipeline: a single `$match` stage. */
export const DEFAULT_PIPELINE: Stage[] = [newStage()];

/** Render a result document in MongoDB shell syntax (`ObjectId('...')`,
 * `ISODate('...')`, …). The backend sends documents as extended JSON
 * (`{"$oid":"..."}`); deserialize that back to BSON types, then stringify the
 * shell representation. Falls back to plain JSON so one odd document can't break
 * the whole result view. */
export function formatDoc(doc: unknown): string {
  try {
    return (
      toJSString(EJSON.deserialize(doc as Record<string, unknown>), 2) ??
      JSON.stringify(doc, null, 2)
    );
  } catch {
    return JSON.stringify(doc, null, 2);
  }
}

/** Render a result document as raw (relaxed) extended JSON, pretty-printed —
 * dates/numbers as plain JSON values, typed values as `{ "$oid": … }` etc.
 * (the form MongoDB tools import). Used by the document "Copy raw JSON" action. */
export function formatDocJson(doc: unknown): string {
  try {
    return EJSON.stringify(EJSON.deserialize(doc as Record<string, unknown>), null, 2, {
      relaxed: true,
    });
  } catch {
    return JSON.stringify(doc, null, 2);
  }
}

/** Pull the documents out of a `find`/`aggregate`/`listIndexes` command reply
 * (they live under `cursor.firstBatch`). */
export function cursorBatch(rc: RunCommandResult): unknown[] {
  const cursor = (rc.result as { cursor?: { firstBatch?: unknown } } | null)?.cursor;
  return Array.isArray(cursor?.firstBatch) ? (cursor!.firstBatch as unknown[]) : [];
}

/** Format a `runCommand` reply for the Script tab: a cursor reply
 * (`find`/`aggregate`/`listIndexes`) yields one formatted entry per document —
 * even none, so an empty result never shows the raw cursor envelope; `distinct`
 * yields its values; every other reply (write/count/admin) is a single blob. */
export function formatRunResult(rc: RunCommandResult): string[] {
  const cursor = (rc.result as { cursor?: { firstBatch?: unknown } } | null)?.cursor;
  if (Array.isArray(cursor?.firstBatch)) {
    return (cursor!.firstBatch as unknown[]).map(formatDoc);
  }
  const values = (rc.result as { values?: unknown } | null)?.values;
  if (Array.isArray(values)) return values.map((v) => formatDoc(v));
  return [formatDoc(rc.result)];
}

/** A single row in the Indexes table. */
export interface IndexRow {
  name: string;
  keys: string;
  props: string;
}

/** Summarize a raw `listIndexes` entry (extended JSON) for the Indexes table. */
export function describeIndex(raw: unknown): IndexRow {
  const idx = EJSON.deserialize(raw as Record<string, unknown>) as Record<string, unknown>;
  const key = (idx.key ?? {}) as Record<string, unknown>;
  const keys = Object.entries(key)
    .map(([k, v]) => `${k}: ${v}`)
    .join(", ");
  const props: string[] = [];
  if (idx.unique) props.push("unique");
  if (idx.sparse) props.push("sparse");
  if (idx.hidden) props.push("hidden");
  if (idx.expireAfterSeconds != null) props.push(`TTL ${idx.expireAfterSeconds}s`);
  if (idx.partialFilterExpression) props.push("partial");
  return { name: String(idx.name ?? ""), keys, props: props.join(", ") || "—" };
}

/** Parse a single aggregation stage body into a value the command can carry.
 * Bare numbers (`$limit: 10`) pass straight through; everything else is parsed
 * as MongoDB shell syntax (objects, arrays, quoted strings). Throws on invalid
 * input so the caller can surface it. */
function parseStageValue(text: string): unknown {
  const t = text.trim();
  if (!t) return {};
  if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
  return parseFilter(t);
}

// --- runCommand builders ---------------------------------------------------
// Each returns the command document as extended JSON (EJSON keeps the command
// name as the first field and encodes typed values canonically). They throw on
// invalid shell-syntax input so the caller can show the error.

/** Build a `find` command from the Documents filter box (shell syntax). */
export function buildFindCommand(collection: string, filterText: string, limit: number): string {
  const text = filterText.trim();
  const filter = text ? parseFilter(text) : {};
  return EJSON.stringify({ find: collection, filter, limit, batchSize: limit }, { relaxed: false });
}

/** Build an `aggregate` command from the builder's enabled, non-empty stages. */
export function buildAggregateCommand(
  collection: string,
  pipeline: Stage[],
  limit: number,
): string {
  const stages = pipeline
    .filter((s) => s.enabled && s.body.trim())
    .map((s) => ({ [s.op]: parseStageValue(s.body) }));
  return EJSON.stringify(
    { aggregate: collection, pipeline: stages, cursor: { batchSize: limit } },
    { relaxed: false },
  );
}

/** Build a `listIndexes` command for a collection. */
export function buildListIndexesCommand(collection: string): string {
  return EJSON.stringify({ listIndexes: collection }, { relaxed: false });
}

// --- mongosh-script builders -----------------------------------------------
// Produce the shell-script form of a query (what the Script tab runs / accepts),
// so the Documents and Aggregation tabs can "Copy query" as a runnable script.

/** `db.coll` for a valid identifier, else `db.getCollection("coll")`. */
function dbCollExpr(collection: string): string {
  return /^[A-Za-z_$][\w$]*$/.test(collection)
    ? `db.${collection}`
    : `db.getCollection(${JSON.stringify(collection)})`;
}

/** A mongosh `find(...).limit(n)` script for the Documents filter (shell syntax). */
export function findScript(collection: string, filterText: string, limit: number): string {
  const filter = filterText.trim() || "{}";
  return `${dbCollExpr(collection)}.find(${filter}).limit(${limit});`;
}

/** A mongosh `aggregate([...])` script from the builder's enabled, non-empty
 * stages (shell syntax bodies preserved). */
export function aggregateScript(collection: string, pipeline: Stage[]): string {
  const stages = pipeline
    .filter((s) => s.enabled && s.body.trim())
    .map((s) => `  { ${s.op}: ${s.body.trim()} }`);
  return `${dbCollExpr(collection)}.aggregate([\n${stages.join(",\n")}\n]);`;
}

// --- per-document editing (Documents tab, Compass-style) -------------------

/** The `_id` of a result document (deserialized from extended JSON), or null if
 * it has none. */
export function docId(doc: unknown): unknown {
  try {
    const d = EJSON.deserialize(doc as Record<string, unknown>) as Record<string, unknown>;
    return d?._id ?? null;
  } catch {
    return null;
  }
}

/** Build a `delete`-by-_id command for one result document. Throws if it has
 * no `_id`. */
export function buildDeleteCommand(collection: string, doc: unknown): string {
  const id = docId(doc);
  if (id === null || id === undefined) {
    throw new Error("Document has no _id to delete by.");
  }
  return EJSON.stringify(
    { delete: collection, deletes: [{ q: { _id: id }, limit: 1 }] },
    { relaxed: false },
  );
}

/** Build a replace command from edited shell-syntax text: the edited document
 * fully replaces the one matched by its `_id`. Throws on parse error or a
 * missing/edited `_id`. */
export function buildReplaceCommand(collection: string, editedText: string): string {
  const replacement = parseFilter(editedText.trim() || "{}") as Record<string, unknown>;
  const id = replacement._id;
  if (id === undefined) {
    throw new Error("The edited document must keep its _id.");
  }
  return EJSON.stringify(
    {
      update: collection,
      updates: [{ q: { _id: id }, u: replacement, multi: false }],
    },
    { relaxed: false },
  );
}
