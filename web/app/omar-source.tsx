"use client";

import { useMemo } from "react";
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
