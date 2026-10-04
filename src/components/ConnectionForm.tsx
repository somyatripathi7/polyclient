import { useMemo, useState } from "react";
import { api, errString, plainSecret } from "../api/api.ts";
import type { PluginInfo, ConfigField, ConnectionConfig, SecretField } from "../api/api.ts";
import type { SavedConnection } from "../api/store.ts";
import { genId } from "../api/store.ts";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";

interface ConnectionFormProps {
  plugins: PluginInfo[];
  error: string | null;
  /** All saved profiles, used to enforce unique connection names. */
  existing: SavedConnection[];
  /** When editing an existing saved profile; null/undefined for a new one. */
  initial?: SavedConnection | null;
  /** When cloning: seeds the form (type/name/config) as a NEW connection. Unlike
   * `initial`, this does not put the form in edit mode — the type stays
   * selectable and a fresh profile is saved. */
  prefill?: SavedConnection | null;
  /** Persist the profile and open a live connection. Should reject on failure
   * so the form can surface the error (the profile is still saved). */
  onSaveAndConnect: (profile: SavedConnection) => Promise<void>;
}

function defaultValues(fields: ConfigField[]): ConnectionConfig {
  const out: ConnectionConfig = {};
  for (const f of fields) {
    if (f.default !== undefined && f.default !== null) out[f.key] = f.default;
    else if (f.type.kind === "boolean") out[f.key] = false;
    // Password fields carry a `SecretField` ({ type, value }) rather than a
    // bare string, so the value is self-describing about how it's stored.
    else if (f.type.kind === "password") out[f.key] = plainSecret("");
    else if (f.type.kind === "keyvalue") out[f.key] = {};
    else out[f.key] = "";
  }
  return out;
}

/** Read a `SecretField`-shaped value back as its plaintext for display. */
function secretValue(value: unknown): string {
  return (value as SecretField | undefined)?.value ?? "";
}

/** A field shows unless its `show_if` rule is unmet by the current values. */
function isVisible(field: ConfigField, values: ConnectionConfig): boolean {
  if (!field.show_if) return true;
  return String(values[field.show_if.field] ?? "") === field.show_if.equals;
}

export function ConnectionForm({
  plugins,
  error,
  existing,
  initial,
  prefill,
  onSaveAndConnect,
}: ConnectionFormProps) {
  // `initial` (edit) takes precedence; otherwise `prefill` (clone) seeds the
  // form. Only `initial` puts the form in edit mode.
  const seed = initial ?? prefill ?? null;
  const [pluginId, setPluginId] = useState<string>(seed?.pluginId ?? "");
  const selected = useMemo(
    () => plugins.find((p) => p.id === pluginId) ?? null,
    [plugins, pluginId],
  );
  const [name, setName] = useState<string>(seed?.name ?? "");
  // Seed values from the saved config, but layer it over the schema defaults so
  // fields absent from an older saved config (e.g. a controller field like
  // `mode` added after the profile was saved) still resolve their value. Without
  // this, `show_if` conditions keyed on a missing field hide their dependents.
  const [values, setValues] = useState<ConnectionConfig>(() => {
    if (!seed) return {};
    const schema = plugins.find((p) => p.id === seed.pluginId)?.config_schema;
    return schema ? { ...defaultValues(schema), ...seed.config } : { ...seed.config };
  });
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ kind: "error" | "ok"; text: string } | null>(null);
  const editing = Boolean(initial);

  function selectPlugin(id: string) {
    setPluginId(id);
    setMsg(null);
    const p = plugins.find((x) => x.id === id);
    setValues(p ? defaultValues(p.config_schema) : {});
  }

  function setField(key: string, value: unknown) {
    setValues((v) => ({ ...v, [key]: value }));
  }

  function coerce(field: ConfigField, raw: string): unknown {
    if (field.type.kind === "number") return raw === "" ? "" : Number(raw);
    // Wrap secrets so the stored config keeps the `{ type, value }` shape.
    if (field.type.kind === "password") return plainSecret(raw);
    return raw;
  }

  /** Auto-derived label used when the user leaves the name blank. */
  function buildLabel(): string {
    const v = values as Record<string, unknown>;
    const host = v.host ?? v.uri ?? v.database;
    return selected ? `${selected.name}${host ? " · " + String(host) : ""}` : "connection";
  }

  /** Only the fields currently shown, used both for rendering and submit. */
  function visibleFields(): ConfigField[] {
    return selected ? selected.config_schema.filter((f) => isVisible(f, values)) : [];
  }

  /** Config to send: only visible fields, so hidden defaults aren't included. */
  function visibleConfig(): ConnectionConfig {
    return Object.fromEntries(visibleFields().map((f) => [f.key, values[f.key]]));
  }

  async function onTest() {
    if (!selected) return;
    setBusy(true);
    setMsg(null);
    try {
      await api.testConnection(selected.id, visibleConfig());
      setMsg({ kind: "ok", text: "Connection succeeded." });
    } catch (e) {
      setMsg({ kind: "error", text: errString(e) });
    } finally {
      setBusy(false);
    }
  }

  async function onSaveAndConnectClick() {
    if (!selected) return;
    const finalName = name.trim() || buildLabel();
    // Names must be unique across saved profiles (case-insensitive), so the
    // sidebar stays unambiguous. Skip the profile being edited.
    const clash = existing.some(
      (c) => c.id !== initial?.id && c.name.trim().toLowerCase() === finalName.toLowerCase(),
    );
    if (clash) {
      setMsg({
        kind: "error",
        text: `A connection named "${finalName}" already exists.`,
      });
      return;
    }
    setBusy(true);
    setMsg(null);
    const profile: SavedConnection = {
      id: initial?.id ?? genId(),
      name: finalName,
      pluginId: selected.id,
      config: visibleConfig(),
    };
    try {
      await onSaveAndConnect(profile);
      // On success the parent navigates away from the form.
    } catch (e) {
      setMsg({ kind: "error", text: errString(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="conn-form">
      <h1>{editing ? "Edit connection" : "New connection"}</h1>
      {error && <div className="msg error">{error}</div>}
      <label className="field">
        <span className="field-label">Type</span>
        {/* The plugin type is fixed once a profile is saved — its config schema
         * (and the live connection mapping) is tied to it, so editing only
         * changes the connection's settings, not its backend. */}
        <select value={pluginId} disabled={editing} onChange={(e) => selectPlugin(e.target.value)}>
          <option value="" disabled>
            Select a connection type…
          </option>
          {plugins.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      </label>
      {selected && (
        <>
          <p className="muted">{selected.description}</p>
          <label className="field">
            <span className="field-label">Connection name</span>
            <input
              type="text"
              value={name}
              placeholder={buildLabel()}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          {visibleFields().map((f) => (
            <Field
              key={f.key}
              field={f}
              value={values[f.key]}
              onChange={(val) => setField(f.key, val)}
              coerce={coerce}
            />
          ))}
          {msg && <div className={"msg " + msg.kind}>{msg.text}</div>}
          <div className="form-actions">
            <button disabled={busy} onClick={onTest}>
              Test
            </button>
            <button className="primary" disabled={busy} onClick={onSaveAndConnectClick}>
              Save &amp; Connect
            </button>
          </div>
        </>
      )}
    </div>
  );
}

function Field({
  field,
  value,
  onChange,
  coerce,
}: {
  field: ConfigField;
  value: unknown;
  onChange: (v: unknown) => void;
  coerce: (f: ConfigField, raw: string) => unknown;
}) {
  const t = field.type;
  return (
    <label className="field">
      <span className="field-label">
        {field.label}
        {field.required && <span className="req">*</span>}
      </span>
      {t.kind === "boolean" ? (
        <input
          type="checkbox"
          checked={Boolean(value)}
          onChange={(e) => onChange(e.target.checked)}
        />
      ) : t.kind === "select" ? (
        <select value={String(value ?? "")} onChange={(e) => onChange(e.target.value)}>
          {t.options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </select>
      ) : t.kind === "filepath" ? (
        <div className="file-path-field">
          <input
            type="text"
            readOnly
            value={value === undefined || value === null ? "" : String(value)}
            placeholder={field.placeholder ?? "No file selected"}
          />
          <button
            type="button"
            onClick={async () => {
              const selected = await openFileDialog({ multiple: false, directory: false });
              if (selected) onChange(selected);
            }}
          >
            Browse…
          </button>
        </div>
      ) : t.kind === "keyvalue" ? (
        <KeyValueEditor
          value={(value as Record<string, string> | undefined) ?? {}}
          onChange={onChange}
        />
      ) : (
        <input
          type={t.kind === "password" ? "password" : t.kind === "number" ? "number" : "text"}
          value={
            t.kind === "password"
              ? secretValue(value)
              : value === undefined || value === null
                ? ""
                : String(value)
          }
          placeholder={field.placeholder ?? ""}
          onChange={(e) => onChange(coerce(field, e.target.value))}
        />
      )}
    </label>
  );
}

function KeyValueEditor({
  value,
  onChange,
}: {
  value: Record<string, string>;
  onChange: (v: Record<string, string>) => void;
}) {
  const entries = Object.entries(value);
  return (
    <div className="keyvalue-editor">
      {entries.length === 0 && <p className="muted keyvalue-empty">No entries yet.</p>}
      {entries.map(([key, val]) => (
        <div key={key} className="keyvalue-row">
          <input
            type="text"
            className="keyvalue-key"
            value={key}
            placeholder="key"
            onChange={(e) => {
              const nextKey = e.target.value;
              const { [key]: removed, ...rest } = value;
              onChange({ ...rest, [nextKey]: removed ?? val });
            }}
          />
          <input
            type="text"
            className="keyvalue-val"
            value={val}
            placeholder="value"
            onChange={(e) => onChange({ ...value, [key]: e.target.value })}
          />
          <button
            type="button"
            className="keyvalue-remove"
            title="Remove"
            onClick={(event) => {
              event.preventDefault();
              const { [key]: _removed, ...rest } = value;
              onChange(rest);
            }}
          >
            ×
          </button>
        </div>
      ))}
      <button
        type="button"
        className="keyvalue-add"
        onClick={() => {
          let n = 1;
          let key = "key";
          while (key in value) {
            n += 1;
            key = `key${n}`;
          }
          onChange({ ...value, [key]: "" });
        }}
      >
        + Add
      </button>
    </div>
  );
}
