/**
 * A tokenizer for OMAR source.
 *
 * No highlighting library ships an OMAR grammar, so either way the grammar has
 * to be written by hand — this keeps it to one small file instead of a
 * dependency plus a custom language definition.
 */

export type TokenKind =
  | "comment"
  | "string"
  | "interpolation"
  | "keyword"
  | "type"
  | "agent"
  | "number"
  | "punctuation"
  | "text";

export type Token = { kind: TokenKind; text: string };

const KEYWORDS = new Set(["team", "input", "output", "action", "prompt"]);
const TYPES = new Set([
  "bool",
  "int",
  "float",
  "string",
  "path",
  "bytes",
  "list",
  "option",
]);

const IDENT_START = /[A-Za-z_]/;
const IDENT_PART = /[A-Za-z0-9_]/;

export function tokenizeOmar(source: string): Token[] {
  const tokens: Token[] = [];
  let index = 0;

  const push = (kind: TokenKind, text: string) => {
    if (!text) return;
    const last = tokens[tokens.length - 1];
    if (last && last.kind === kind) last.text += text;
    else tokens.push({ kind, text });
  };

  while (index < source.length) {
    const char = source[index];
    const rest = source.slice(index);

    if (rest.startsWith("//")) {
      const end = source.indexOf("\n", index);
      const stop = end === -1 ? source.length : end;
      push("comment", source.slice(index, stop));
      index = stop;
      continue;
    }

    // Block comments nest: the corpus exercises `/* … /* … */ … */`.
    if (rest.startsWith("/*")) {
      let depth = 0;
      let cursor = index;
      while (cursor < source.length) {
        if (source.startsWith("/*", cursor)) {
          depth += 1;
          cursor += 2;
        } else if (source.startsWith("*/", cursor)) {
          depth -= 1;
          cursor += 2;
          if (depth === 0) break;
        } else {
          cursor += 1;
        }
      }
      push("comment", source.slice(index, cursor));
      index = cursor;
      continue;
    }

    // Prompt bodies. `$( … )` escapes back out to an expression.
    if (char === '"') {
      let cursor = index + 1;
      push("string", '"');
      while (cursor < source.length && source[cursor] !== '"') {
        if (source.startsWith("$(", cursor)) {
          const close = source.indexOf(")", cursor);
          const stop = close === -1 ? source.length : close + 1;
          push("interpolation", source.slice(cursor, stop));
          cursor = stop;
          continue;
        }
        push("string", source[cursor]);
        cursor += 1;
      }
      if (cursor < source.length) {
        push("string", '"');
        cursor += 1;
      }
      index = cursor;
      continue;
    }

    if (IDENT_START.test(char)) {
      let cursor = index;
      while (cursor < source.length && IDENT_PART.test(source[cursor])) cursor += 1;
      const word = source.slice(index, cursor);
      if (KEYWORDS.has(word)) push("keyword", word);
      else if (TYPES.has(word)) push("type", word);
      // Backends are the only capitalised names in practice (Codex, ClaudeCode).
      else if (/^[A-Z]/.test(word)) push("agent", word);
      else push("text", word);
      index = cursor;
      continue;
    }

    if (/[0-9]/.test(char)) {
      let cursor = index;
      while (cursor < source.length && /[0-9.]/.test(source[cursor])) cursor += 1;
      push("number", source.slice(index, cursor));
      index = cursor;
      continue;
    }

    if (/[(){}[\],:;|?<>=-]/.test(char)) {
      push("punctuation", char);
      index += 1;
      continue;
    }

    push("text", char);
    index += 1;
  }

  return tokens;
}
