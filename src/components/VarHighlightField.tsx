import { useRef } from "react";
import { splitVarTokens } from "../api/curlui.ts";

/** Render a template's `{{NAME}}` tokens as colored spans (resolved/unresolved
 *  against `env`), for the backdrop layer of {@link VarHighlightInput} /
 *  {@link VarHighlightTextarea}. Plain runs render as-is. */
function renderTokens(value: string, env: Record<string, string>) {
  return splitVarTokens(value).map((t, i) =>
    t.isVar ? (
      <span
        key={i}
        className={"var-hl-token " + (t.name! in env ? "var-hl-resolved" : "var-hl-unresolved")}
      >
        {t.text}
      </span>
    ) : (
      <span key={i}>{t.text}</span>
    ),
  );
}

/** A single-line `<input>` that highlights `{{NAME}}` placeholders inline,
 *  colored by whether `NAME` resolves in `env`. Implemented as a transparent
 *  input layered over a backdrop `<div>` rendering the same text with colored
 *  spans — native inputs can't style substrings directly. The backdrop is
 *  purely visual (`aria-hidden`, `pointer-events: none`); the real input still
 *  drives value/selection/caret/focus. */
export function VarHighlightInput({
  value,
  onChange,
  env,
  className,
  placeholder,
  type = "text",
  onPaste,
  title,
}: {
  value: string;
  onChange: (value: string) => void;
  env: Record<string, string>;
  className?: string;
  placeholder?: string;
  type?: string;
  onPaste?: (e: React.ClipboardEvent<HTMLInputElement>) => void;
  title?: string;
}) {
  const backdropRef = useRef<HTMLDivElement>(null);
  return (
    <div className={"var-hl-field" + (className ? " " + className : "")} title={title}>
      <div ref={backdropRef} className="var-hl-backdrop" aria-hidden="true">
        {renderTokens(value, env)}
      </div>
      <input
        type={type}
        className="var-hl-input"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        onPaste={onPaste}
        onScroll={(e) => {
          if (backdropRef.current) backdropRef.current.scrollLeft = e.currentTarget.scrollLeft;
        }}
      />
    </div>
  );
}

/** Multi-line `<textarea>` counterpart of {@link VarHighlightInput}, for
 *  templated request bodies. Syncs both scroll axes between the real textarea
 *  and its backdrop. */
export function VarHighlightTextarea({
  value,
  onChange,
  env,
  className,
  placeholder,
  spellCheck,
}: {
  value: string;
  onChange: (value: string) => void;
  env: Record<string, string>;
  className?: string;
  placeholder?: string;
  spellCheck?: boolean;
}) {
  const backdropRef = useRef<HTMLDivElement>(null);
  // A trailing newline needs a phantom extra line to match a real <textarea>'s
  // rendered height (browsers render one blank line after it); a lone trailing
  // space achieves that without being visible.
  const backdropValue = value.endsWith("\n") ? value + " " : value;
  return (
    <div className={"var-hl-field" + (className ? " " + className : "")}>
      <div ref={backdropRef} className="var-hl-backdrop multiline" aria-hidden="true">
        {renderTokens(backdropValue, env)}
      </div>
      <textarea
        className="var-hl-input"
        value={value}
        placeholder={placeholder}
        spellCheck={spellCheck}
        onChange={(e) => onChange(e.target.value)}
        onScroll={(e) => {
          const b = backdropRef.current;
          if (!b) return;
          b.scrollLeft = e.currentTarget.scrollLeft;
          b.scrollTop = e.currentTarget.scrollTop;
        }}
      />
    </div>
  );
}
