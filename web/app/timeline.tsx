"use client";

import type { TimelineStep } from "./lib/runtime-client";

/**
 * The logical timeline of a program: every tag it passes through, in order.
 *
 * Before anything is deployed these are projected — worked out from the program
 * rather than observed, which is possible because what decides a tag is the
 * program and not what an agent says. Stepping through them is the determinism
 * claim made checkable: this is what will happen, and here it is before it has.
 *
 * Once a run is live the same strip follows it. Same steps, same order; what
 * changes is that a position is now a fact rather than a prediction, and the
 * strip moves on its own. That the two are the same list is the point — a live
 * run that departed from its projection would be visible here as a mismatch,
 * rather than being invisible because projection and observation were drawn by
 * different things.
 *
 * An input arriving mid-run makes the tail of this wrong, so the projection is
 * recomputed and the strip redrawn from where the run actually is.
 */
export function Timeline({
  steps,
  index,
  live,
  truncated,
  onScrub,
  onClose,
}: {
  steps: TimelineStep[];
  /** Which step is showing; the run's own position when live. */
  index: number;
  /** Following a run rather than being scrubbed by hand. */
  live: boolean;
  /** The projection stopped early. The program has not. */
  truncated: boolean;
  onScrub: (index: number) => void;
  onClose: () => void;
}) {
  const step = steps[index];
  const last = steps.length - 1;

  return (
    <div className="timeline" aria-label="Logical timeline">
      <div className="timeline-controls">
        <button
          type="button"
          aria-label="Previous tag"
          disabled={index <= 0}
          onClick={() => onScrub(Math.max(0, index - 1))}
        >
          ‹
        </button>
        <input
          type="range"
          min={0}
          max={Math.max(0, last)}
          value={index}
          aria-label="Logical tag"
          disabled={steps.length === 0}
          onChange={(event) => onScrub(Number(event.target.value))}
        />
        <button
          type="button"
          aria-label="Next tag"
          disabled={index >= last}
          onClick={() => onScrub(Math.min(last, index + 1))}
        >
          ›
        </button>
        <button type="button" className="timeline-close" onClick={onClose}>
          Hide
        </button>
      </div>

      <div className="timeline-readout">
        {steps.length === 0 ? (
          <span className="timeline-idle">
            Nothing to project: no input is set and no timer fires, so the
            program does not move.
          </span>
        ) : (
          <>
            <span className="timeline-tag">
              {/* The tag itself, which is what "when" means here — there is no
                  wall clock in it. */}
              {step.timestamp}:{step.microstep}
            </span>
            <span className="timeline-count">
              {index + 1} of {steps.length}
              {truncated ? "+" : ""}
            </span>
            <span className={live ? "timeline-mode live" : "timeline-mode"}>
              {live ? "live" : "projected"}
            </span>
            <span className="timeline-detail">
              {step.reactions.length > 0
                ? `${step.reactions.length} reaction${step.reactions.length > 1 ? "s" : ""} fire`
                : "no reaction fires"}
              {step.events.length > 0 ? ` · ${step.events.join(", ")}` : ""}
            </span>
          </>
        )}
      </div>

      {truncated ? (
        <p className="timeline-truncated">
          Stopped after {steps.length} tags. A periodic timer has no end, so a
          preview has to.
        </p>
      ) : null}
    </div>
  );
}
