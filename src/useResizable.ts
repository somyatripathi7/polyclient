// Reusable drag-to-resize handle.
//
// Generalizes the outer sidebar's resize logic (see `startSidebarResize` in
// App.tsx) so workspace panels can share it. `onChange` fires live during the
// drag (update local size state); `onCommit` fires once on mouseup (good place
// to persist the final size).
//
// Defaults to a horizontal handle (drag right widens the panel to its left).
// Pass `axis: "y"` for a horizontal handle that sits *below* a panel: dragging
// down grows the panel above it (used for the editor/results vertical split).

import type React from "react";

/** Min/max width (px) for a workspace's resizable tree / scripts panel. */
export const TREE_MIN = 160;
export const TREE_MAX = 560;

/** Min/max height (px) for a workspace's resizable editor pane. */
export const EDITOR_MIN = 90;
export const EDITOR_MAX = 700;

interface ResizableOptions {
  /** Current size in CSS px (the drag starts from here). */
  width: number;
  /** Clamp bounds in CSS px. */
  min: number;
  max: number;
  /** Live size during the drag. */
  onChange: (width: number) => void;
  /** Final size on mouseup (e.g. to persist). */
  onCommit: (width: number) => void;
  /** Drag axis: "x" (default, horizontal resize) or "y" (vertical resize). */
  axis?: "x" | "y";
}

/** Returns an `onMouseDown` to attach to a resize-handle element. For axis "x"
 *  the handle sits to the right of the panel, so dragging right widens it; for
 *  axis "y" it sits below the panel, so dragging down grows it. */
export function useResizable({
  width,
  min,
  max,
  onChange,
  onCommit,
  axis = "x",
}: ResizableOptions): { onMouseDown: (e: React.MouseEvent) => void } {
  function onMouseDown(e: React.MouseEvent) {
    e.preventDefault();
    const start = axis === "y" ? e.clientY : e.clientX;
    const startSize = width;
    let last = startSize;
    function onMove(ev: MouseEvent) {
      const pos = axis === "y" ? ev.clientY : ev.clientX;
      last = Math.min(max, Math.max(min, startSize + pos - start));
      onChange(last);
    }
    function onUp() {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      onCommit(last);
    }
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    document.body.style.cursor = axis === "y" ? "row-resize" : "col-resize";
  }
  return { onMouseDown };
}
