import { useMemo } from "react";

interface Token {
  text: string;
  kind?: "kw" | "str" | "num" | "cmt" | "fn" | "type" | "op";
}

const PYTHON_KEYWORDS = new Set([
  "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from",
  "as", "try", "except", "finally", "with", "raise", "pass", "break", "continue",
  "and", "or", "not", "is", "in", "lambda", "yield", "async", "await", "True", "False", "None"
]);

const RUST_KEYWORDS = new Set([
  "fn", "pub", "struct", "enum", "impl", "trait", "let", "mut", "if", "else", "match",
  "loop", "while", "for", "in", "return", "break", "continue", "use", "mod", "crate",
  "self", "super", "where", "async", "await", "unsafe", "type", "const", "static", "true", "false"
]);

const JS_KEYWORDS = new Set([
  "function", "const", "let", "var", "return", "if", "else", "switch", "case", "for",
  "while", "import", "export", "from", "default", "class", "extends", "new", "this",
  "async", "await", "try", "catch", "finally", "throw", "typeof", "instanceof", "true", "false", "null", "undefined"
]);

export function tokenizeLine(line: string, path: string): Token[] {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  const isPython = ext === "py";
  const isRust = ext === "rs";
  const isJs = ext === "ts" || ext === "tsx" || ext === "js" || ext === "jsx";
  const keywords = isPython ? PYTHON_KEYWORDS : isRust ? RUST_KEYWORDS : isJs ? JS_KEYWORDS : null;

  if (!keywords || !line) {
    return [{ text: line }];
  }

  const tokens: Token[] = [];
  let i = 0;
  const len = line.length;

  while (i < len) {
    // Single-line comment
    if ((isPython && line[i] === "#") || (!isPython && line.slice(i, i + 2) === "//")) {
      tokens.push({ text: line.slice(i), kind: "cmt" });
      break;
    }

    // Strings
    if (line[i] === '"' || line[i] === "'" || line[i] === '`') {
      const quote = line[i];
      let j = i + 1;
      while (j < len && line[j] !== quote) {
        if (line[j] === "\\" && j + 1 < len) j++;
        j++;
      }
      if (j < len) j++;
      tokens.push({ text: line.slice(i, j), kind: "str" });
      i = j;
      continue;
    }

    // Numbers
    if (/\d/.test(line[i]!) && (i === 0 || /[^a-zA-Z0-9_]/.test(line[i - 1]!))) {
      let j = i;
      while (j < len && /[0-9a-fA-FxX_.]/.test(line[j]!)) j++;
      tokens.push({ text: line.slice(i, j), kind: "num" });
      i = j;
      continue;
    }

    // Words / Identifiers
    if (/[a-zA-Z_]/.test(line[i]!)) {
      let j = i;
      while (j < len && /[a-zA-Z0-9_]/.test(line[j]!)) j++;
      const word = line.slice(i, j);

      if (keywords.has(word)) {
        tokens.push({ text: word, kind: "kw" });
      } else if (j < len && line[j] === "(") {
        tokens.push({ text: word, kind: "fn" });
      } else if (/^[A-Z][a-zA-Z0-9]*$/.test(word)) {
        tokens.push({ text: word, kind: "type" });
      } else {
        tokens.push({ text: word });
      }
      i = j;
      continue;
    }

    // Operators / punctuation
    let j = i;
    while (j < len && !/[a-zA-Z0-9_"'`#\s]/.test(line[j]!) && !(line.slice(j, j + 2) === "//")) {
      j++;
    }
    if (j === i) {
      // Whitespace
      tokens.push({ text: line[i]! });
      i++;
    } else {
      tokens.push({ text: line.slice(i, j), kind: "op" });
      i = j;
    }
  }

  return tokens;
}

export function HighlightedLine({ line, path }: { line: string; path: string }) {
  const tokens = useMemo(() => tokenizeLine(line, path), [line, path]);
  return (
    <span>
      {tokens.map((t, idx) =>
        t.kind ? (
          <span key={idx} className={`tok-${t.kind}`}>
            {t.text}
          </span>
        ) : (
          <span key={idx}>{t.text}</span>
        ),
      )}
    </span>
  );
}
