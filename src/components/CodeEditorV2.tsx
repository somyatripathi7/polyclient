import { useEffect, useImperativeHandle, useRef } from "react";
import { Compartment, EditorState, Prec } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  MatchDecorator,
  ViewPlugin,
  keymap,
  placeholder as placeholderExt,
} from "@codemirror/view";
import { indentWithTab } from "@codemirror/commands";
import {
  StreamLanguage,
  HighlightStyle,
  syntaxHighlighting,
  defaultHighlightStyle,
} from "@codemirror/language";
import { basicSetup } from "codemirror";
import { sql } from "@codemirror/lang-sql";
import { json } from "@codemirror/lang-json";
import { javascript } from "@codemirror/lang-javascript";
import { html } from "@codemirror/lang-html";
import { xml } from "@codemirror/lang-xml";
import { shell } from "@codemirror/legacy-modes/mode/shell";

export type CodeLanguage = "sql" | "bash" | "json" | "javascript" | "xml" | "html";

/** A keybinding the host layers on top of the editor's own. The `key` is a
 *  CodeMirror key name (e.g. "Mod-Enter", "Escape"); `run` is invoked when it
 *  fires and the event is always consumed. The *set* of keys is captured when
 *  the editor mounts (callers use a stable set), but each `run` closure is read
 *  fresh on every keystroke, so up-to-date handlers always fire. */
export interface EditorKeybinding {
  key: string;
  run: () => void;
}

/** Imperative handle exposed via {@link CodeEditorV2Props.handleRef} for callers
 *  that need to read selection state or focus the editor (e.g. run-selection in
 *  the SQL/CLI workspaces). The textarea-era callers reached into a real
 *  `<textarea>`; these methods are the CodeMirror equivalents. */
export interface CodeEditorV2Handle {
  focus(): void;
  getValue(): string;
  /** The selected text, or "" when the selection is empty. */
  getSelection(): string;
  /** The full text of the line the primary cursor is on. */
  getCursorLine(): string;
  /** The document offset of the primary cursor's head. */
  getCursorOffset(): number;
  /** The underlying CodeMirror view, or null before mount / after unmount. */
  readonly view: EditorView | null;
}

export interface CodeEditorV2Props {
  value: string;
  onChange?: (value: string) => void;
  /** Grammar to highlight with; omit for a plain (un-highlighted) editor. */
  language?: CodeLanguage;
  /** Applied to the wrapper element for per-editor sizing (e.g. "code"). */
  className?: string;
  placeholder?: string;
  readOnly?: boolean;
  autoFocus?: boolean;
  /** Soft-wrap long lines instead of scrolling horizontally. */
  lineWrapping?: boolean;
  /** Host shortcuts layered above the editor's own keymap (consumed when hit). */
  keybindings?: EditorKeybinding[];
  handleRef?: React.Ref<CodeEditorV2Handle>;
  /** When set, `{{NAME}}` placeholders in the document are highlighted,
   *  colored by whether `NAME` resolves in this map. */
  highlightVars?: Record<string, string>;
}

/** CodeMirror-side chrome (selection, caret, gutters) that's awkward to express
 *  from external CSS because CodeMirror's own base theme has high specificity.
 *  Structural layout/sizing lives in styles.css against the `.cm-*` classes. */
const baseTheme = EditorView.theme(
  {
    "&": { backgroundColor: "transparent", color: "var(--text)" },
    ".cm-content": { caretColor: "var(--text)" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--text)" },
    "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection":
      { backgroundColor: "color-mix(in srgb, var(--accent) 38%, transparent)" },
    ".cm-gutters": {
      backgroundColor: "var(--bg-2)",
      color: "var(--muted)",
      border: "none",
    },
    ".cm-activeLine": { backgroundColor: "color-mix(in srgb, var(--accent) 9%, transparent)" },
    ".cm-activeLineGutter": {
      backgroundColor: "color-mix(in srgb, var(--accent) 14%, transparent)",
    },
    ".cm-foldPlaceholder": {
      backgroundColor: "var(--bg-3)",
      color: "var(--muted)",
      border: "1px solid var(--border)",
    },
    "&.cm-focused": { outline: "none" },
    ".cm-matchingBracket, &.cm-focused .cm-matchingBracket": {
      backgroundColor: "color-mix(in srgb, var(--accent) 22%, transparent)",
      color: "inherit",
    },
    ".cm-selectionMatch": {
      backgroundColor: "color-mix(in srgb, var(--warn) 22%, transparent)",
    },
    ".cm-panels": {
      backgroundColor: "var(--bg-2)",
      color: "var(--text)",
      borderColor: "var(--border)",
    },
    ".cm-panels.cm-panels-top": { borderBottom: "1px solid var(--border)" },
    ".cm-panels.cm-panels-bottom": { borderTop: "1px solid var(--border)" },
    // The find/replace panel (⌘/Ctrl+F): CodeMirror's defaults are light-theme
    // colors that read as black-on-black here, so restyle inputs/buttons to the
    // app's tokens.
    ".cm-textfield": {
      backgroundColor: "var(--bg-3)",
      color: "var(--text)",
      border: "1px solid var(--border)",
      borderRadius: "4px",
    },
    ".cm-textfield:focus": { outline: "none", borderColor: "var(--accent)" },
    ".cm-button": {
      backgroundColor: "var(--bg-3)",
      backgroundImage: "none",
      color: "var(--text)",
      border: "1px solid var(--border)",
      borderRadius: "4px",
    },
    ".cm-button:hover": {
      backgroundColor: "color-mix(in srgb, var(--accent) 18%, var(--bg-3))",
    },
    ".cm-button:active": {
      backgroundColor: "color-mix(in srgb, var(--accent) 30%, var(--bg-3))",
    },
    ".cm-panel.cm-search label": { color: "var(--muted)" },
    ".cm-panel.cm-search input[type=checkbox]": { accentColor: "var(--accent)" },
    ".cm-panel button[name=close]": { color: "var(--muted)" },
    // {{NAME}} placeholder highlighting (see `variableHighlightExtension`),
    // colored by whether NAME resolves in the env passed as `highlightVars`.
    ".cm-var-token": { borderRadius: "3px" },
    ".cm-var-resolved": {
      color: "var(--accent-2)",
      backgroundColor: "color-mix(in srgb, var(--accent-2) 20%, transparent)",
    },
    ".cm-var-unresolved": {
      color: "var(--err)",
      backgroundColor: "color-mix(in srgb, var(--err) 18%, transparent)",
    },
  },
  { dark: true },
);

/** Syntax colors that track the app's theme CSS variables, so highlighting stays
 *  legible on every (dark) theme instead of CodeMirror's light-tuned
 *  `defaultHighlightStyle` — whose dark token colors render near-invisible on our
 *  dark editor background (e.g. `false`, object keys).
 *
 *  Rather than author a fresh tag→color list (which would require the `tags`
 *  object, only shipped by `@lezer/highlight`), we reuse `defaultHighlightStyle`'s
 *  own specs — they already carry the `Tag` objects — and just remap each spec's
 *  hard-coded light color onto a theme variable. This keeps the proper tag-based
 *  API with no direct `@lezer/highlight` dependency. */
const TOKEN_COLORS: Record<string, string> = {
  "#404740": "var(--muted)", // meta
  "#708": "var(--accent)", // keyword
  "#219": "var(--warn)", // atom, bool, labelName  (was invisible)
  "#164": "var(--accent-2)", // literal / number
  "#a11": "var(--ok)", // string
  "#e40": "var(--ok)", // regexp / escape / special string
  "#00f": "var(--text)", // definition(variableName)
  "#30a": "var(--text)", // local(variableName)
  "#085": "var(--warn)", // typeName / namespace
  "#167": "var(--warn)", // className
  "#256": "var(--accent-2)", // special(variableName) / macroName
  "#00c": "var(--text)", // definition(propertyName)  (was invisible)
  "#940": "var(--muted)", // comment
  "#f00": "var(--err)", // invalid
};

const highlightStyle = HighlightStyle.define(
  // Specs with no color (link/heading/emphasis/strong/strikethrough) pass
  // through unchanged; an unmapped color falls back to itself.
  defaultHighlightStyle.specs.map((spec) =>
    spec.color ? { ...spec, color: TOKEN_COLORS[spec.color] ?? spec.color } : spec,
  ),
);

/** The grammar extension for a language (empty for plain text). */
function languageExtension(language?: CodeLanguage) {
  switch (language) {
    case "sql":
      return sql();
    case "json":
      return json();
    case "javascript":
      return javascript();
    case "html":
      return html();
    case "xml":
      return xml();
    case "bash":
      return StreamLanguage.define(shell);
    default:
      return [];
  }
}

const VAR_TOKEN_RE = /\{\{\s*[^{}]*\s*\}\}/g;

/** Highlights `{{NAME}}` placeholders, colored by whether `NAME` (trimmed)
 *  resolves in `env` — mirrors the same-purpose overlay used on plain
 *  `<input>`/`<textarea>` fields (`VarHighlightField`), but as a native
 *  CodeMirror decoration since this editor already has its own render pass.
 *  Empty (no `env`) disables the extension entirely. */
function variableHighlightExtension(env?: Record<string, string>) {
  if (!env) return [];
  const matcher = new MatchDecorator({
    regexp: VAR_TOKEN_RE,
    decoration: (match) => {
      const name = match[0].slice(2, -2).trim();
      const resolved = name in env;
      return Decoration.mark({
        class: "cm-var-token " + (resolved ? "cm-var-resolved" : "cm-var-unresolved"),
      });
    },
  });
  return ViewPlugin.define(
    (view) => ({
      decorations: matcher.createDeco(view),
      update(update) {
        this.decorations = matcher.updateDeco(update, this.decorations);
      },
    }),
    { decorations: (v) => v.decorations },
  );
}

/**
 * A CodeMirror 6 code editor — the successor to the transparent-textarea overlay
 * in CodeEditorV1. CodeMirror provides syntax highlighting, line numbers, code
 * folding, bracket matching, auto-close, smart indent, and a find panel
 * (⌘/Ctrl+F) natively, replacing the hand-rolled equivalents the old editor
 * stack carried.
 *
 * The value is controlled: edits flow out through `onChange`, and an incoming
 * `value` that differs from the current document is reconciled with a minimal
 * change. Callers that need selection state or focus use the {@link
 * CodeEditorV2Handle} via `handleRef`; host shortcuts are added through
 * `keybindings`.
 */
export function CodeEditorV2({
  value,
  onChange,
  language,
  className,
  placeholder,
  readOnly = false,
  autoFocus = false,
  lineWrapping = false,
  keybindings,
  handleRef,
  highlightVars,
}: CodeEditorV2Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);

  // Read the latest props from inside long-lived CodeMirror callbacks without
  // tearing down and rebuilding the editor on every render.
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const keybindingsRef = useRef(keybindings);
  keybindingsRef.current = keybindings;

  // Set immediately before a reconcile-effect dispatch (below) and read inside
  // the updateListener, which CodeMirror invokes synchronously within
  // `dispatch()` — so this never straddles a render. Distinguishes "the doc
  // changed because a new `value` prop arrived" (e.g. switching to a
  // different request reusing this editor instance) from a real user edit:
  // only the latter should flow back out through `onChange`, otherwise the
  // host sees a no-op "edit" and marks the record dirty.
  const suppressOnChangeRef = useRef(false);

  // Compartments let us reconfigure individual facets (language, read-only,
  // placeholder) without recreating the view.
  const languageComp = useRef(new Compartment());
  const readOnlyComp = useRef(new Compartment());
  const placeholderComp = useRef(new Compartment());
  const highlightVarsComp = useRef(new Compartment());

  // Build the editor once. Subsequent prop changes are pushed in via the
  // effects below (or the compartments), so this never re-runs.
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    // Bind the keys present at mount; each handler reads the current closure
    // fresh so up-to-date callbacks fire even though the binding is fixed.
    const hostKeys = (keybindingsRef.current ?? []).map((kb) => ({
      key: kb.key,
      preventDefault: true,
      run: () => {
        keybindingsRef.current?.find((k) => k.key === kb.key)?.run();
        return true;
      },
    }));

    const state = EditorState.create({
      doc: value,
      extensions: [
        basicSetup,
        Prec.highest(keymap.of([...hostKeys, indentWithTab])),
        languageComp.current.of(languageExtension(language)),
        // `readOnly` blocks edits; keep the editor `editable` (focusable) so the
        // find panel (⌘/Ctrl+F) and text selection still work when read-only.
        readOnlyComp.current.of([EditorState.readOnly.of(readOnly), EditorView.editable.of(true)]),
        placeholderComp.current.of(placeholder ? placeholderExt(placeholder) : []),
        highlightVarsComp.current.of(variableHighlightExtension(highlightVars)),
        lineWrapping ? EditorView.lineWrapping : [],
        Prec.highest(syntaxHighlighting(highlightStyle)),
        baseTheme,
        EditorView.updateListener.of((u) => {
          if (!u.docChanged) return;
          if (suppressOnChangeRef.current) {
            suppressOnChangeRef.current = false;
            return;
          }
          onChangeRef.current?.(u.state.doc.toString());
        }),
      ],
    });

    const view = new EditorView({ state, parent: host });
    viewRef.current = view;
    if (autoFocus) view.focus();

    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // Intentionally mount-once; prop updates are handled by the effects below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Reconcile external value changes into the document (skips no-ops, so typing
  // doesn't loop: edit → onChange → parent setState → value === doc → skip).
  // This dispatch is a resync, not a user edit (e.g. a different request's body
  // swapped into a reused editor instance), so it's flagged to skip onChange —
  // otherwise the host would see a spurious "edit" back to the value it just
  // handed in and mark the record dirty.
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (value !== current) {
      suppressOnChangeRef.current = true;
      view.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: languageComp.current.reconfigure(languageExtension(language)),
    });
  }, [language]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: readOnlyComp.current.reconfigure([
        EditorState.readOnly.of(readOnly),
        EditorView.editable.of(true),
      ]),
    });
  }, [readOnly]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: placeholderComp.current.reconfigure(placeholder ? placeholderExt(placeholder) : []),
    });
  }, [placeholder]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: highlightVarsComp.current.reconfigure(variableHighlightExtension(highlightVars)),
    });
  }, [highlightVars]);

  useImperativeHandle(
    handleRef,
    (): CodeEditorV2Handle => ({
      focus: () => viewRef.current?.focus(),
      getValue: () => viewRef.current?.state.doc.toString() ?? "",
      getSelection: () => {
        const view = viewRef.current;
        if (!view) return "";
        const { from, to } = view.state.selection.main;
        return view.state.sliceDoc(from, to);
      },
      getCursorLine: () => {
        const view = viewRef.current;
        if (!view) return "";
        return view.state.doc.lineAt(view.state.selection.main.head).text;
      },
      getCursorOffset: () => viewRef.current?.state.selection.main.head ?? 0,
      get view() {
        return viewRef.current;
      },
    }),
    [],
  );

  return <div ref={hostRef} className={"code-editor-v2" + (className ? " " + className : "")} />;
}
