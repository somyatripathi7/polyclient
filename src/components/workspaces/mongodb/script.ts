import {
  EJSON,
  ObjectId,
  Long,
  Int32,
  Double,
  Decimal128,
  Timestamp,
  UUID,
  Binary,
  MinKey,
  MaxKey,
} from "bson";
import { errString } from "../../../api/api.ts";
import type { RunCommandResult } from "../../../api/document.ts";
import type { StatementResult } from "./mongo.ts";
import { cursorBatch, formatDoc } from "./mongo.ts";
import { normalizeQuotes } from "../rdbms/sql.ts";

// Runtime for the MongoDB workspace's "Script" tab. Rather than parse
// `db.<collection>.<method>(...)` with regexes, we provide a real `db` object
// whose collections and methods are ordinary JavaScript functions, and execute
// each statement as JavaScript with `db` (plus the shell BSON constructors) in
// scope. JS parses the arguments and handles chaining/nesting natively; each
// method builds a MongoDB command document and runs it through the existing
// `document.run_command` op (one call per method invocation).
//
// Execution is statement-by-statement (see splitStatements) so results can be
// shown per statement and writes run in order. Known limitations: variables do
// not carry across statements, and cursors only return their first batch (no
// getMore), so a `.limit()` bounds what `.toArray()` yields.
//
// SECURITY: scripts run as JavaScript in the app's renderer. Only run scripts
// you trust (your own). Common ambient globals are shadowed (see evalStatement)
// as a guard rail, but this is not a security sandbox.

/** Runs a command document, returning the raw reply. Bound by the caller to a
 * connection + database. */
type RunFn = (commandJson: string) => Promise<RunCommandResult>;

/** Internal: execute a built command, recording it for per-statement display. */
type ExecFn = (command: object, collection: string, method: string) => Promise<RunCommandResult>;

interface ExecEntry {
  command: string;
  collection: string;
  method: string;
  rc?: RunCommandResult;
  error?: string;
  promise: Promise<RunCommandResult>;
}

/** Execute a whole script, returning one or more results per statement. `run`
 * is bound to the target connection/database; `dbName` is that database (exposed
 * as `db.getName()`); `defaultLimit` bounds find/aggregate batches when the
 * script doesn't set its own `.limit()`. `onProgress` is called with the
 * accumulating results as each statement completes. */
export async function executeScript(
  src: string,
  defaultLimit: number,
  dbName: string,
  run: RunFn,
  onProgress?: (results: StatementResult[]) => void,
): Promise<StatementResult[]> {
  const results: StatementResult[] = [];
  const statements = splitStatements(normalizeQuotes(src));
  for (let i = 0; i < statements.length; i++) {
    const index = i + 1;
    const source = statements[i].text.trim();
    const collector: ExecEntry[] = [];

    const exec: ExecFn = (command, collection, method) => {
      const commandJson = EJSON.stringify(command, { relaxed: false });
      const entry: ExecEntry = {
        command: commandJson,
        collection,
        method,
        promise: undefined as unknown as Promise<RunCommandResult>,
      };
      entry.promise = run(commandJson).then(
        (rc) => {
          entry.rc = rc;
          return rc;
        },
        (err) => {
          entry.error = errString(err);
          throw err;
        },
      );
      collector.push(entry);
      return entry.promise;
    };

    let threw: string | undefined;
    let value: unknown;
    try {
      value = await evalStatement(source, createRuntime(exec, defaultLimit, dbName));
    } catch (e) {
      threw = errString(e);
    }
    // Settle any commands the statement fired but didn't await, so the next
    // statement runs after this one's effects land.
    await Promise.allSettled(collector.map((e) => e.promise));

    if (collector.length > 0) {
      for (const e of collector) {
        results.push({
          index,
          source,
          collection: e.collection,
          method: e.method,
          command: e.command,
          result: e.rc,
          error: e.error,
        });
      }
    } else if (threw) {
      results.push({
        index,
        source,
        collection: "",
        method: "",
        command: "",
        error: threw,
      });
    } else if (value !== undefined) {
      results.push({
        index,
        source,
        collection: "",
        method: "",
        command: "",
        value: formatDoc(value),
      });
    }
    onProgress?.([...results]);
  }
  return results;
}

// --- statement splitting ---------------------------------------------------

/** Split a script into top-level statements. Splits on `;` at nesting depth 0,
 * and on a newline at depth 0 unless the next significant token is `.` (so a
 * find with a chained modifier on the next line stays one statement). Tracks
 * strings, `(){}[]` nesting, and line/block comments; drops fragments that are
 * only whitespace/comments. `start`/`end` are the fragment's character range in
 * `src`. Mirrors the SQL splitter in rdbms/sql.ts. */
export function splitStatements(src: string): { text: string; start: number; end: number }[] {
  const out: { text: string; start: number; end: number }[] = [];
  const n = src.length;
  let i = 0;
  let start = 0;
  let depth = 0;
  const push = (end: number) => {
    const raw = src.slice(start, end);
    const stripped = raw
      .replace(/\/\/[^\n]*/g, "")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .trim();
    if (stripped) out.push({ text: raw, start, end });
  };
  while (i < n) {
    const c = src[i];
    const c2 = src[i + 1];
    if (c === "'" || c === '"' || c === "`") {
      i = skipString(src, i);
    } else if (c === "/" && c2 === "/") {
      i += 2;
      while (i < n && src[i] !== "\n") i++;
    } else if (c === "/" && c2 === "*") {
      i += 2;
      while (i < n && !(src[i] === "*" && src[i + 1] === "/")) i++;
      i += 2;
    } else if (c === "(" || c === "{" || c === "[") {
      depth++;
      i++;
    } else if (c === ")" || c === "}" || c === "]") {
      depth = Math.max(0, depth - 1);
      i++;
    } else if (depth === 0 && c === ";") {
      push(i);
      i++;
      start = i;
    } else if (depth === 0 && c === "\n" && !continuesChain(src, i + 1)) {
      push(i);
      i++;
      start = i;
    } else {
      i++;
    }
  }
  push(n);
  return out;
}

/** The statement spanning `cursor` (the first whose range ends at or after the
 * cursor), trimmed. Falls back to the whole trimmed script. Mirrors
 * `statementAtCursor` in rdbms/sql.ts but uses the Mongo-script splitter. */
export function statementAtCursor(src: string, cursor: number): string {
  const ranges = splitStatements(normalizeQuotes(src));
  if (ranges.length === 0) return src.trim();
  let chosen = ranges[ranges.length - 1];
  for (const r of ranges) {
    if (cursor <= r.end) {
      chosen = r;
      break;
    }
  }
  return chosen.text.trim();
}

/** Index just past a string literal whose opening quote is at `i`. */
function skipString(src: string, i: number): number {
  const q = src[i];
  const n = src.length;
  i++;
  while (i < n) {
    if (src[i] === "\\") {
      i += 2;
      continue;
    }
    if (src[i] === q) return i + 1;
    i++;
  }
  return i;
}

/** Whether the next significant token at/after `p` (skipping whitespace and
 * comments) is a `.`, i.e. a chained call continuing the previous line. */
function continuesChain(src: string, p: number): boolean {
  const n = src.length;
  let i = p;
  while (i < n) {
    const c = src[i];
    const c2 = src[i + 1];
    if (c === " " || c === "\t" || c === "\r" || c === "\n") {
      i++;
    } else if (c === "/" && c2 === "/") {
      i += 2;
      while (i < n && src[i] !== "\n") i++;
    } else if (c === "/" && c2 === "*") {
      i += 2;
      while (i < n && !(src[i] === "*" && src[i + 1] === "/")) i++;
      i += 2;
    } else {
      return c === ".";
    }
  }
  return false;
}

// --- statement evaluation --------------------------------------------------

/** Ambient globals shadowed (set to `undefined`) inside the script function as
 * a guard rail. Not a real sandbox — `Function` reachability can't be removed.
 * `eval`/`arguments` are intentionally omitted: they're illegal parameter names
 * in strict mode and would break every statement. */
const BLOCKED_GLOBALS = [
  "window",
  "globalThis",
  "self",
  "document",
  "fetch",
  "XMLHttpRequest",
  "WebSocket",
  "importScripts",
  "require",
  "process",
  "__TAURI__",
  "__TAURI_INTERNALS__",
];

/** Evaluate one statement with the runtime context in scope, returning its
 * value (awaited). Tries the statement as a returnable expression first, then
 * falls back to a statement body (for declarations/control flow). */
async function evalStatement(source: string, ctx: Record<string, unknown>): Promise<unknown> {
  const names = [...Object.keys(ctx), ...BLOCKED_GLOBALS];
  const values = [...Object.values(ctx), ...BLOCKED_GLOBALS.map(() => undefined)];
  let fn: (...args: unknown[]) => unknown;
  try {
    fn = new Function(...names, `"use strict"; return (\n${source}\n);`) as typeof fn;
  } catch {
    fn = new Function(...names, `"use strict";\n${source}\n`) as typeof fn;
  }
  return await fn(...values);
}

// --- the `db` runtime ------------------------------------------------------

/** Build the script scope: a `db` object plus the shell BSON constructors. */
function createRuntime(
  exec: ExecFn,
  defaultLimit: number,
  dbName: string,
): Record<string, unknown> {
  // Database-level operations (mirroring the popular mongosh `db.*` helpers).
  // Each builds a command document and runs it via exec; the label "db" makes
  // the results panel read `db.<method>()`. Accessing any other property of
  // `db` returns a Collection handle (so `db.users` works), exactly like the
  // shell — use `db.getCollection(name)` for a collection that shadows a helper.
  const target = {
    getName: () => dbName,
    getCollection: (name: string) => new Collection(name, exec, defaultLimit),
    /** Run a raw command document, mirroring `db.runCommand(...)`. */
    runCommand: (command: object) => exec(command, "db", "runCommand").then((rc) => rc.result),
    /** Collection names in the database (via `listCollections` nameOnly). */
    getCollectionNames: () =>
      exec({ listCollections: 1, nameOnly: true }, "db", "getCollectionNames")
        .then(cursorBatch)
        .then((cs) => (cs as { name?: string }[]).map((c) => c.name)),
    /** Full `listCollections` entries, optionally filtered. */
    getCollectionInfos: (filter: object = {}) =>
      exec(prune({ listCollections: 1, filter }), "db", "getCollectionInfos").then(cursorBatch),
    createCollection: (name: string, opts: object = {}) =>
      exec({ create: name, ...opts }, "db", "createCollection").then((rc) => rc.result),
    dropDatabase: () => exec({ dropDatabase: 1 }, "db", "dropDatabase").then((rc) => rc.result),
    stats: (scale?: number) =>
      exec(prune({ dbStats: 1, scale }), "db", "stats").then((rc) => rc.result),
    serverStatus: () => exec({ serverStatus: 1 }, "db", "serverStatus").then((rc) => rc.result),
    hostInfo: () => exec({ hostInfo: 1 }, "db", "hostInfo").then((rc) => rc.result),
    ping: () => exec({ ping: 1 }, "db", "ping").then((rc) => rc.result),
    version: () =>
      exec({ buildInfo: 1 }, "db", "version").then(
        (rc) => (rc.result as { version?: string } | null)?.version ?? null,
      ),
  };
  const db = new Proxy(target, {
    get(t, prop) {
      if (typeof prop !== "string") return undefined;
      if (prop in t) return (t as Record<string, unknown>)[prop];
      return new Collection(prop, exec, defaultLimit);
    },
  });
  return {
    db,
    ObjectId: (hex?: string) => (hex ? new ObjectId(hex) : new ObjectId()),
    ISODate: (s?: string) => (s ? new Date(s) : new Date()),
    Date: (s?: string) => (s ? new Date(s) : new Date()),
    UUID: (s?: string) => (s ? new UUID(s) : new UUID()),
    NumberLong: (v: string | number) => Long.fromString(String(v)),
    NumberInt: (v: string | number) => new Int32(Number(v)),
    NumberDecimal: (v: string | number) => Decimal128.fromString(String(v)),
    NumberDouble: (v: string | number) => new Double(Number(v)),
    Timestamp: (t = 0, i = 0) => new Timestamp({ t, i }),
    BinData: (subtype: number, base64: string) => Binary.createFromBase64(base64, subtype),
    MinKey: () => new MinKey(),
    MaxKey: () => new MaxKey(),
  };
}

/** Drop keys whose value is `undefined` so they aren't sent in the command. */
function prune<T extends Record<string, unknown>>(obj: T): T {
  for (const k of Object.keys(obj)) if (obj[k] === undefined) delete obj[k];
  return obj;
}

/** Forward only a vetted subset of write options into the command. */
const UPDATE_OPTS = ["arrayFilters", "collation", "hint"] as const;

function buildUpdate(
  c: string,
  f: unknown,
  u: unknown,
  opts: Record<string, unknown>,
  multi: boolean,
): object {
  const update: Record<string, unknown> = {
    q: f ?? {},
    u,
    upsert: opts.upsert === true,
    multi,
  };
  for (const k of UPDATE_OPTS) if (opts[k] !== undefined) update[k] = opts[k];
  return { update: c, updates: [update] };
}

function autoIndexName(keys: Record<string, unknown>): string {
  return Object.entries(keys)
    .map(([k, v]) => `${k}_${v}`)
    .join("_");
}

/** A collection handle: its methods build and run MongoDB commands. Return
 * values mirror the shell loosely (documents for reads, the raw reply for
 * writes) so scripts can use them; the results panel shows the command + reply
 * regardless. */
class Collection {
  constructor(
    private name: string,
    private exec: ExecFn,
    private defaultLimit: number,
  ) {}

  find(filter: object = {}, projection?: object): Cursor {
    return new Cursor(this.name, filter, projection, this.exec, this.defaultLimit);
  }

  async findOne(filter: object = {}, projection?: object): Promise<unknown> {
    const rc = await this.exec(
      prune({ find: this.name, filter, projection, limit: 1, singleBatch: true }),
      this.name,
      "findOne",
    );
    return cursorBatch(rc)[0] ?? null;
  }

  countDocuments(filter: object = {}): Promise<unknown> {
    return this.exec({ count: this.name, query: filter }, this.name, "countDocuments").then(
      (rc) => (rc.result as { n?: number } | null)?.n ?? 0,
    );
  }

  count(filter: object = {}): Promise<unknown> {
    return this.countDocuments(filter);
  }

  distinct(key: string, query: object = {}): Promise<unknown> {
    return this.exec({ distinct: this.name, key, query }, this.name, "distinct").then(
      (rc) => (rc.result as { values?: unknown[] } | null)?.values ?? [],
    );
  }

  aggregate(pipeline: object[] = [], opts: object = {}): Promise<unknown> {
    return this.exec(
      {
        aggregate: this.name,
        pipeline,
        ...opts,
        cursor: { batchSize: this.defaultLimit },
      },
      this.name,
      "aggregate",
    ).then(cursorBatch);
  }

  insertOne(doc: object): Promise<unknown> {
    return this.exec({ insert: this.name, documents: [doc] }, this.name, "insertOne").then(
      (rc) => rc.result,
    );
  }

  insertMany(docs: object[]): Promise<unknown> {
    if (!Array.isArray(docs)) {
      throw new Error("insertMany() expects an array of documents.");
    }
    return this.exec({ insert: this.name, documents: docs }, this.name, "insertMany").then(
      (rc) => rc.result,
    );
  }

  updateOne(f: object, u: object, opts: object = {}): Promise<unknown> {
    return this.exec(
      buildUpdate(this.name, f, u, opts as Record<string, unknown>, false),
      this.name,
      "updateOne",
    ).then((rc) => rc.result);
  }

  updateMany(f: object, u: object, opts: object = {}): Promise<unknown> {
    return this.exec(
      buildUpdate(this.name, f, u, opts as Record<string, unknown>, true),
      this.name,
      "updateMany",
    ).then((rc) => rc.result);
  }

  replaceOne(f: object, r: object, opts: object = {}): Promise<unknown> {
    return this.exec(
      buildUpdate(this.name, f, r, opts as Record<string, unknown>, false),
      this.name,
      "replaceOne",
    ).then((rc) => rc.result);
  }

  deleteOne(f: object = {}): Promise<unknown> {
    return this.exec(
      { delete: this.name, deletes: [{ q: f, limit: 1 }] },
      this.name,
      "deleteOne",
    ).then((rc) => rc.result);
  }

  deleteMany(f: object = {}): Promise<unknown> {
    return this.exec(
      { delete: this.name, deletes: [{ q: f, limit: 0 }] },
      this.name,
      "deleteMany",
    ).then((rc) => rc.result);
  }

  createIndex(keys: object, opts: object = {}): Promise<unknown> {
    const o = opts as Record<string, unknown>;
    const index: Record<string, unknown> = {
      key: keys,
      name: typeof o.name === "string" ? o.name : autoIndexName(keys as Record<string, unknown>),
    };
    for (const [k, v] of Object.entries(o)) if (k !== "name") index[k] = v;
    return this.exec({ createIndexes: this.name, indexes: [index] }, this.name, "createIndex").then(
      (rc) => rc.result,
    );
  }

  dropIndex(index: string | object): Promise<unknown> {
    return this.exec({ dropIndexes: this.name, index }, this.name, "dropIndex").then(
      (rc) => rc.result,
    );
  }

  getIndexes(): Promise<unknown> {
    return this.exec({ listIndexes: this.name }, this.name, "getIndexes").then(cursorBatch);
  }

  drop(): Promise<unknown> {
    return this.exec({ drop: this.name }, this.name, "drop").then((rc) => rc.result);
  }
}

/** A chainable find cursor. Accumulates modifiers and runs the `find` (or a
 * `count`) when awaited or via `.toArray()`/`.forEach()`. Only the first batch
 * is returned (no getMore), so `.limit()` bounds what is yielded. */
class Cursor {
  private _limit?: number;
  private _skip?: number;
  private _sort?: object;
  private _projection?: object;
  private _count = false;

  constructor(
    private coll: string,
    private filter: object,
    projection: object | undefined,
    private exec: ExecFn,
    private defaultLimit: number,
  ) {
    this._projection = projection;
  }

  limit(n: number): this {
    this._limit = n;
    return this;
  }
  skip(n: number): this {
    this._skip = n;
    return this;
  }
  sort(s: object): this {
    this._sort = s;
    return this;
  }
  projection(p: object): this {
    this._projection = p;
    return this;
  }
  count(): this {
    this._count = true;
    return this;
  }

  private run(): Promise<unknown> {
    if (this._count) {
      return this.exec(
        prune({
          count: this.coll,
          query: this.filter,
          skip: this._skip,
          limit: this._limit,
        }),
        this.coll,
        "count",
      ).then((rc) => (rc.result as { n?: number } | null)?.n ?? 0);
    }
    const lim = this._limit ?? this.defaultLimit;
    return this.exec(
      prune({
        find: this.coll,
        filter: this.filter,
        projection: this._projection,
        sort: this._sort,
        skip: this._skip,
        limit: lim,
        batchSize: lim,
      }),
      this.coll,
      "find",
    ).then(cursorBatch);
  }

  toArray(): Promise<unknown> {
    return this.run();
  }
  forEach(fn: (doc: unknown) => void): Promise<void> {
    return this.run().then((docs) => (docs as unknown[]).forEach(fn));
  }
  // Thenable: `await cursor` (or a bare cursor statement) runs the query.
  then<T>(onFulfilled?: (value: unknown) => T, onRejected?: (reason: unknown) => T): Promise<T> {
    return this.run().then(onFulfilled, onRejected);
  }
  catch<T>(onRejected?: (reason: unknown) => T): Promise<unknown> {
    return this.run().catch(onRejected);
  }
}
