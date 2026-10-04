// Session-scoped, per-connection workspace state.
//
// Switching the active connection in App unmounts the previous workspace
// (renderWorkspace only renders the active one, keyed by the live connection
// id), so all in-component React state is otherwise lost on switch. This module
// holds that state in a store *outside* the React tree so a workspace can be
// unmounted and remounted — as happens on every connection switch — and
// rehydrate exactly where it left off.
//
// In-memory for the session only: nothing here is persisted to disk. State is
// dropped when a connection is closed (see `clearConnectionState`), so a
// reconnect starts fresh.

import { useCallback, useState } from "react";

/** field -> last value, for one (connection, workspace-kind) scope. */
type Bag = Record<string, unknown>;

/** scope key ("<savedId>::<kind>") -> its field bag. */
const store = new Map<string, Bag>();

/** Build the scope key for a connection's workspace. Kind is part of the key so
 * two different plugins for the same profile never collide on a field name. */
export const ConnScope = (savedId: string, kind: string): string => `${savedId}::${kind}`;

/**
 * A drop-in replacement for `useState` whose value is mirrored into the
 * session store under `(scope, field)`. On mount it restores the stored value
 * if one exists, else uses `initial`; every update writes through to the store.
 *
 * `scope` should come from {@link ConnScope}. `field` is a stable key unique
 * within the scope.
 */
export function useConnectionState<T>(
  scope: string,
  field: string,
  initial: T,
): [T, (next: T | ((prev: T) => T)) => void] {
  const [value, setValue] = useState<T>(() => {
    const bag = store.get(scope);
    return bag && field in bag ? (bag[field] as T) : initial;
  });

  const set = useCallback(
    (next: T | ((prev: T) => T)) => {
      setValue((prev) => {
        const resolved = typeof next === "function" ? (next as (p: T) => T)(prev) : next;
        const bag = store.get(scope) ?? {};
        bag[field] = resolved;
        store.set(scope, bag);
        return resolved;
      });
    },
    [scope, field],
  );

  return [value, set];
}

/** Drop all stored workspace state for a connection profile (every kind/scope).
 * Called when a connection is closed/deleted so it doesn't leak into a later
 * reconnect — but NOT on a mere connection switch, which is what we preserve. */
export function clearConnectionState(savedId: string): void {
  const prefix = savedId + "::";
  for (const key of [...store.keys()]) {
    if (key.startsWith(prefix)) store.delete(key);
  }
}
