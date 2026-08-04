"use client";

import { KeyboardEvent, PointerEvent as ReactPointerEvent, useRef } from "react";

/**
 * A draggable column divider that doubles as the column's collapse control.
 *
 * Dragging a panel to nothing hides it and leaves this behind as a slim edge
 * handle, so the layout has no separate show/hide buttons — the divider is the
 * whole control. It reports the offset from where a drag began rather than the
 * pointer position, so the caller clamps against its own layout and the handle
 * stays ignorant of the panels around it.
 */
export function Resizer({
  label,
  collapsed,
  toward,
  onExpand,
  onDragStart,
  onDelta,
  onStep,
}: {
  label: string;
  collapsed: boolean;
  /** Which way the hidden panel reappears. */
  toward: "left" | "right";
  onExpand: () => void;
  onDragStart: () => void;
  onDelta: (deltaX: number) => void;
  onStep: (deltaX: number) => void;
}) {
  const originRef = useRef<number | null>(null);

  if (collapsed) {
    return (
      <button
        type="button"
        className={`resizer-handle ${toward}`}
        onClick={onExpand}
        aria-label={`Show ${label}`}
        title={`Show ${label}`}
      >
        <span aria-hidden="true">{toward === "right" ? "›" : "‹"}</span>
      </button>
    );
  }

  function handlePointerDown(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) return;
    originRef.current = event.clientX;
    onDragStart();
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function handlePointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    if (originRef.current === null) return;
    onDelta(event.clientX - originRef.current);
  }

  function endDrag(event: ReactPointerEvent<HTMLDivElement>) {
    if (originRef.current === null) return;
    originRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }

  function handleKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const step = event.shiftKey ? 64 : 16;
    if (event.key === "ArrowLeft") onStep(-step);
    else if (event.key === "ArrowRight") onStep(step);
    else return;
    event.preventDefault();
  }

  return (
    <div
      className="resizer"
      role="separator"
      aria-orientation="vertical"
      aria-label={`Resize ${label}`}
      tabIndex={0}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onKeyDown={handleKeyDown}
    >
      <i aria-hidden="true" />
    </div>
  );
}
