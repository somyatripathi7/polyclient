// Sandboxed JavaScript runner for curlui pre-request / test scripts.
//
// User scripts are arbitrary JavaScript, so they run inside a QuickJS WASM VM
// (via quickjs-emscripten) with no access to the DOM, Tauri, or the network —
// only the `client` API defined by the bootstrap below. Full chai is available
// as `client.expect` by evaluating chai's UMD bundle inside the VM once.
//
// The VM context is created lazily and reused across runs (chai is ~360 KB —
// evaluating it per send would be slow). The bootstrap re-seeds every per-run
// global so nothing leaks between runs. A hard timeout via the runtime interrupt
// handler aborts infinite loops; on any VM-level failure the context is disposed
// and recreated on the next call.

import {
  newQuickJSWASMModuleFromVariant,
  type QuickJSContext,
  type QuickJSWASMModule,
} from "quickjs-emscripten-core";
import releaseSyncVariant from "@jitl/quickjs-wasmfile-release-sync";
import chaiSource from "chai/chai.js?raw";

export interface ScriptRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  body?: string | null;
}

export interface ScriptResponse {
  code: number;
  status: string;
  headers: Record<string, string>;
  body: string;
  responseTime: number;
}

export interface ScriptInput {
  request: ScriptRequest;
  /** Present only for post-response (test) scripts. */
  response?: ScriptResponse;
  environment: Record<string, string>;
  collectionVariables: Record<string, string>;
  /** Transient per-send locals (client.variables.set), seeded empty. */
  variables: Record<string, string>;
}

export interface TestResult {
  name: string;
  passed: boolean;
  error?: string;
}

export interface LogLine {
  level: string;
  text: string;
}

export interface ScriptOutcome {
  request: ScriptRequest;
  environment: Record<string, string>;
  collectionVariables: Record<string, string>;
  variables: Record<string, string>;
  tests: TestResult[];
  logs: LogLine[];
  /** Set when the script failed to compile, threw at the top level, or timed
   *  out. Individual failed `client.test(...)` assertions are in `tests`. */
  error?: string;
}

const TIMEOUT_MS = 2000;
const MEMORY_LIMIT_BYTES = 64 * 1024 * 1024;

// Defines `client` + `console` and the per-run `__data`/`__results`/`__logs`
// globals on top of an already-set `globalThis.__input`. Re-run every call.
const BOOTSTRAP = `
globalThis.__data = globalThis.__input;
globalThis.__results = [];
globalThis.__logs = [];
(function () {
  var d = globalThis.__data;
  function mkVars(store) {
    return {
      get: function (k) { return store[k]; },
      set: function (k, v) { store[k] = v == null ? "" : String(v); },
      unset: function (k) { delete store[k]; },
      has: function (k) { return Object.prototype.hasOwnProperty.call(store, k); },
      toObject: function () { return Object.assign({}, store); },
      clear: function () { for (var k in store) if (Object.prototype.hasOwnProperty.call(store, k)) delete store[k]; }
    };
  }
  function findCI(key) {
    var lk = String(key).toLowerCase();
    for (var k in d.request.headers) if (k.toLowerCase() === lk) return k;
    return null;
  }
  function removeCI(key) { var m = findCI(key); if (m) delete d.request.headers[m]; }

  var client = {};
  client.environment = mkVars(d.environment);
  client.collectionVariables = mkVars(d.collectionVariables);
  client.variables = {
    get: function (k) {
      if (Object.prototype.hasOwnProperty.call(d.variables, k)) return d.variables[k];
      if (Object.prototype.hasOwnProperty.call(d.environment, k)) return d.environment[k];
      if (Object.prototype.hasOwnProperty.call(d.collectionVariables, k)) return d.collectionVariables[k];
      return undefined;
    },
    set: function (k, v) { d.variables[k] = v == null ? "" : String(v); },
    has: function (k) { return client.variables.get(k) !== undefined; }
  };

  client.request = {
    get method() { return d.request.method; },
    set method(v) { d.request.method = v; },
    get url() { return d.request.url; },
    set url(v) { d.request.url = v; },
    get body() { return d.request.body; },
    set body(v) { d.request.body = v; },
    headers: {
      add: function (h) { if (h && h.key != null) d.request.headers[h.key] = h.value == null ? "" : String(h.value); },
      upsert: function (h) { if (h && h.key != null) { removeCI(h.key); d.request.headers[h.key] = h.value == null ? "" : String(h.value); } },
      remove: function (key) { removeCI(key); },
      get: function (key) { var m = findCI(key); return m ? d.request.headers[m] : undefined; },
      all: function () { return Object.assign({}, d.request.headers); }
    }
  };

  if (d.response) {
    client.response = {
      code: d.response.code,
      status: d.response.status,
      responseTime: d.response.responseTime,
      headers: Object.assign({}, d.response.headers),
      text: function () { return d.response.body; },
      json: function () { return JSON.parse(d.response.body); }
    };
  }

  client.expect = globalThis.chai.expect;
  client.test = function (name, fn) {
    try {
      fn();
      globalThis.__results.push({ name: String(name), passed: true });
    } catch (e) {
      globalThis.__results.push({ name: String(name), passed: false, error: (e && e.message) ? String(e.message) : String(e) });
    }
  };

  globalThis.client = client;

  function logger(level) {
    return function () {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) {
        var x = arguments[i];
        if (typeof x === "string") parts.push(x);
        else { try { parts.push(JSON.stringify(x)); } catch (e) { parts.push(String(x)); } }
      }
      globalThis.__logs.push({ level: level, text: parts.join(" ") });
    };
  }
  globalThis.console = { log: logger("log"), info: logger("info"), warn: logger("warn"), error: logger("error"), debug: logger("log") };
})();
`;

const READOUT = `JSON.stringify({
  environment: __data.environment,
  collectionVariables: __data.collectionVariables,
  variables: __data.variables,
  request: { method: __data.request.method, url: __data.request.url, headers: __data.request.headers, body: __data.request.body },
  tests: __results,
  logs: __logs
})`;

let modPromise: Promise<QuickJSWASMModule> | null = null;
let ctx: QuickJSContext | null = null;
// Wall-clock deadline for the interrupt handler; Infinity outside a user run.
let runDeadline = Infinity;

async function getContext(): Promise<QuickJSContext> {
  if (ctx) return ctx;
  if (!modPromise) modPromise = newQuickJSWASMModuleFromVariant(releaseSyncVariant);
  const QuickJS = await modPromise;
  const c = QuickJS.newContext();
  c.runtime.setMemoryLimit(MEMORY_LIMIT_BYTES);
  c.runtime.setInterruptHandler(() => performance.now() > runDeadline);
  const r = c.evalCode(chaiSource);
  if (r.error) {
    const e = c.dump(r.error);
    r.error.dispose();
    c.dispose();
    throw new Error("chai init failed: " + JSON.stringify(e));
  }
  r.value.dispose();
  ctx = c;
  return ctx;
}

function disposeContext() {
  if (ctx) {
    try {
      ctx.dispose();
    } catch {
      // Leaked handles on a corrupted context — drop the reference regardless.
    }
    ctx = null;
  }
}

function formatError(dumped: unknown): string {
  if (dumped && typeof dumped === "object") {
    const o = dumped as { name?: string; message?: string };
    if (o.message) return o.name ? `${o.name}: ${o.message}` : o.message;
  }
  return String(dumped);
}

/** Evaluate `code`, disposing handles; throw on a VM error (used for our own
 *  bootstrap/readout, which must not fail). Returns the dumped value. */
function evalOrThrow(c: QuickJSContext, code: string): unknown {
  const r = c.evalCode(code);
  if (r.error) {
    const e = c.dump(r.error);
    r.error.dispose();
    throw new Error(formatError(e));
  }
  const v = c.dump(r.value);
  r.value.dispose();
  return v;
}

/** Run a user script against `input` and return the resulting variable state,
 *  request mutations, test results, and console logs. Never rejects. */
export async function runScript(source: string, input: ScriptInput): Promise<ScriptOutcome> {
  const passthrough = (error?: string): ScriptOutcome => ({
    request: input.request,
    environment: input.environment,
    collectionVariables: input.collectionVariables,
    variables: input.variables,
    tests: [],
    logs: [],
    error,
  });

  let c: QuickJSContext;
  try {
    c = await getContext();
  } catch (e) {
    disposeContext();
    return passthrough(e instanceof Error ? e.message : String(e));
  }

  try {
    runDeadline = Infinity;
    // Seed data + (re)define the client API for this run.
    evalOrThrow(c, `globalThis.__input = ${JSON.stringify(input)};`);
    evalOrThrow(c, BOOTSTRAP);

    // Run the user script under the timeout. A compile error / thrown error /
    // interrupt becomes the outcome's `error`, not a thrown exception.
    runDeadline = performance.now() + TIMEOUT_MS;
    let userError: string | undefined;
    const ur = c.evalCode(source);
    if (ur.error) {
      const e = c.dump(ur.error);
      ur.error.dispose();
      userError = formatError(e);
    } else {
      ur.value.dispose();
    }
    runDeadline = Infinity;

    const outJson = evalOrThrow(c, READOUT) as string;
    const out = JSON.parse(outJson) as Omit<ScriptOutcome, "error">;
    return { ...out, error: userError };
  } catch (e) {
    // Bootstrap/readout failed or the VM is corrupted (e.g. OOM, interrupted
    // mid-readout) — recreate the context next time so it can't poison later runs.
    disposeContext();
    return passthrough(e instanceof Error ? e.message : String(e));
  } finally {
    runDeadline = Infinity;
  }
}
