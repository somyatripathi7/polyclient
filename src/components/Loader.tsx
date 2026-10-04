import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from "react";

/** Where a loader overlay paints (and therefore what it blocks). */
export type LoaderScope = "app" | "workspace";

interface LoaderState {
  visible: boolean;
  message: string | null;
  scope: LoaderScope;
}

export interface LoaderApi {
  /** Show the blocking loader. Defaults to `app` scope (covers the window). */
  show: (opts?: { message?: string; scope?: LoaderScope }) => void;
  /** Hide the loader. */
  hide: () => void;
  /** Show the loader for the lifetime of `p`, hiding it when `p` settles. */
  withLoader: <T>(p: Promise<T>, opts?: { message?: string; scope?: LoaderScope }) => Promise<T>;
  /** Current state (read by {@link WorkspaceLoaderSlot} to place the overlay). */
  state: LoaderState;
}

const LoaderContext = createContext<LoaderApi | null>(null);

const HIDDEN: LoaderState = { visible: false, message: null, scope: "app" };

/** The spinner + optional message card shown inside an overlay. */
function LoaderCard({ message }: { message: string | null }) {
  return (
    <div className="loader-card" role="status" aria-live="polite">
      <span className="loader-spinner" />
      {message && <span className="loader-message">{message}</span>}
    </div>
  );
}

/**
 * Provides the app-wide blocking loader. Renders its children, plus — when an
 * `app`-scope loader is showing — a full-window overlay that blocks every click.
 * The `workspace`-scope overlay is rendered separately by
 * {@link WorkspaceLoaderSlot}, which `App` mounts inside `.main` so the sidebar
 * stays interactive (the user can still switch connections).
 */
export function LoaderProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<LoaderState>(HIDDEN);

  const show = useCallback(
    (opts?: { message?: string; scope?: LoaderScope }) =>
      setState({
        visible: true,
        message: opts?.message ?? null,
        scope: opts?.scope ?? "app",
      }),
    [],
  );

  const hide = useCallback(() => setState(HIDDEN), []);

  const withLoader = useCallback(
    async <T,>(p: Promise<T>, opts?: { message?: string; scope?: LoaderScope }): Promise<T> => {
      show(opts);
      try {
        return await p;
      } finally {
        hide();
      }
    },
    [show, hide],
  );

  const api = useMemo<LoaderApi>(
    () => ({ show, hide, withLoader, state }),
    [show, hide, withLoader, state],
  );

  return (
    <LoaderContext.Provider value={api}>
      {children}
      {state.visible && state.scope === "app" && (
        <div className="loader-overlay loader-overlay--app">
          <LoaderCard message={state.message} />
        </div>
      )}
    </LoaderContext.Provider>
  );
}

/**
 * The `workspace`-scope overlay. Mounted inside `.main` (which is
 * `position: relative`) so it covers only the workspace area, leaving the
 * sidebar clickable. Renders nothing unless a `workspace`-scope loader is shown.
 */
export function WorkspaceLoaderSlot() {
  const { state } = useLoader();
  if (!state.visible || state.scope !== "workspace") return null;
  return (
    <div className="loader-overlay loader-overlay--workspace">
      <LoaderCard message={state.message} />
    </div>
  );
}

/** Access the loader controls. Must be used within a {@link LoaderProvider}. */
// eslint-disable-next-line react-refresh/only-export-components
export function useLoader(): LoaderApi {
  const ctx = useContext(LoaderContext);
  if (!ctx) throw new Error("useLoader must be used within a LoaderProvider");
  return ctx;
}
