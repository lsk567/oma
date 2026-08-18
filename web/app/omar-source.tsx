"use client";

import { useMemo, useRef, useState } from "react";
import { tokenizeOmar } from "./lib/omar-syntax";

/** Highlighted OMAR source. Tokenizing is pure, so it is memoised per program. */
export function OmarSource({ source }: { source: string }) {
  const tokens = useMemo(() => tokenizeOmar(source), [source]);
  return (
    <pre className="source-code">
      <code>
        {tokens.map((token, index) => (
          <span key={index} className={`omar-tok-${token.kind}`}>
            {token.text}
          </span>
        ))}
      </code>
    </pre>
  );
}

/**
 * The same source, editable, checked by the compiler as it is typed.
 *
 * A textarea laid transparently over the highlighted text: the operator types
 * into the textarea and reads the colours behind it, which keeps the
 * highlighting without reimplementing a text editor. The two must agree on
 * every metric that decides where a glyph lands, so they share a font, a line
 * height and a padding, and scroll together.
 *
 * Checking is the runtime's own compiler and verifier rather than an
 * approximation of them here: a program this calls valid is one the daemon
 * accepts, and there is no second opinion to keep in step.
 */
export function OmarEditor({
  source,
  filename,
  status,
  errors,
  checking,
  onSourceChange,
  onFilenameChange,
}: {
  source: string;
  filename: string;
  /** What the run is doing, or "draft" before there is one. */
  status: string;
  errors: string[];
  checking: boolean;
  onSourceChange: (source: string) => void;
  onFilenameChange: (filename: string) => void;
}) {
  const tokens = useMemo(() => tokenizeOmar(source), [source]);
  const behind = useRef<HTMLPreElement | null>(null);
  // Null until the operator types: the name shown is the one they were given
  // until it is the one they are writing. Derived rather than synchronised, so
  // a rename in progress is never overwritten by a re-render.
  const [edited, setEdited] = useState<string | null>(null);
  const name = edited ?? filename;

  function commit() {
    setEdited(null);
    onFilenameChange(name);
  }

  return (
    <>
      <div className="source-title">
        <input
          className="source-name"
          value={name}
          spellCheck={false}
          aria-label="Program file name"
          onChange={(event) => setEdited(event.target.value)}
          onBlur={commit}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              commit();
              (event.target as HTMLInputElement).blur();
            }
          }}
        />
        <span>{checking ? "checking…" : status}</span>
      </div>

      <div className="source-editor">
        <pre className="source-code" ref={behind} aria-hidden="true">
          <code>
            {tokens.map((token, index) => (
              <span key={index} className={`omar-tok-${token.kind}`}>
                {token.text}
              </span>
            ))}
            {/* Keeps the last line reachable when it is the one being typed. */}
            {"\n"}
          </code>
        </pre>
        <textarea
          className="source-input"
          value={source}
          spellCheck={false}
          aria-label="OMAR program"
          onChange={(event) => onSourceChange(event.target.value)}
          onKeyDown={(event) => {
            // A textarea gives Tab to the browser, which moves focus out of
            // the editor mid-word. In a code pane it indents.
            if (event.key !== "Tab" || event.metaKey || event.ctrlKey) return;
            event.preventDefault();
            const field = event.currentTarget;
            const { selectionStart, selectionEnd, value } = field;
            const indent = "    ";
            const next = value.slice(0, selectionStart) + indent + value.slice(selectionEnd);
            onSourceChange(next);
            // Put the caret after what was inserted; React re-renders from the
            // value, so this has to happen once that has landed.
            requestAnimationFrame(() => {
              field.selectionStart = selectionStart + indent.length;
              field.selectionEnd = field.selectionStart;
            });
          }}
          onScroll={(event) => {
            const pre = behind.current;
            if (!pre) return;
            pre.scrollTop = event.currentTarget.scrollTop;
            pre.scrollLeft = event.currentTarget.scrollLeft;
          }}
        />
      </div>

      {errors.length > 0 ? (
        <div className="source-errors" role="alert">
          {errors.map((error) => (
            <p key={error}>{error}</p>
          ))}
        </div>
      ) : null}
    </>
  );
}
