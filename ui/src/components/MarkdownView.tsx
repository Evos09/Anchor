import { useMemo } from "react";
import { HighlightedLine } from "./SyntaxHighlighter";

interface MarkdownViewProps {
  content: string;
  className?: string;
}

export function MarkdownView({ content, className = "" }: MarkdownViewProps) {
  const blocks = useMemo(() => parseMarkdown(content), [content]);

  return (
    <div className={`md-body ${className}`}>
      {blocks.map((block, idx) => {
        switch (block.type) {
          case "h1":
            return <h1 key={idx} className="md-h1">{block.content}</h1>;
          case "h2":
            return <h2 key={idx} className="md-h2">{block.content}</h2>;
          case "h3":
            return <h3 key={idx} className="md-h3">{block.content}</h3>;
          case "h4":
            return <h4 key={idx} className="md-h4">{block.content}</h4>;
          case "code":
            return (
              <div key={idx} className="md-codeblock">
                {block.lang && <div className="md-codelang mono">{block.lang}</div>}
                <pre className="pre" style={{ margin: 0 }}>
                  {block.lines.map((line, lIdx) => (
                    <div key={lIdx}>
                      <HighlightedLine line={line} path={`file.${block.lang || "txt"}`} />
                    </div>
                  ))}
                </pre>
              </div>
            );
          case "alert":
            return (
              <div key={idx} className={`banner banner-${block.kind || "info"} md-alert`}>
                <strong>{block.title || block.kind?.toUpperCase()}</strong>
                <p style={{ margin: "0.25rem 0 0" }}>{block.content}</p>
              </div>
            );
          case "list":
            return (
              <ul key={idx} className="md-list">
                {block.items.map((item, iIdx) => (
                  <li key={iIdx} className={item.checked !== undefined ? "md-task-item" : ""}>
                    {item.checked !== undefined && (
                      <span className="todo-box">{item.checked ? "■ " : "□ "}</span>
                    )}
                    <span>{renderInline(item.text)}</span>
                  </li>
                ))}
              </ul>
            );
          case "quote":
            return (
              <blockquote key={idx} className="md-quote">
                {block.content}
              </blockquote>
            );
          case "hr":
            return <hr key={idx} className="md-hr" />;
          case "p":
          default:
            return <p key={idx} className="md-p">{renderInline(block.content)}</p>;
        }
      })}
    </div>
  );
}

interface Block {
  type: "h1" | "h2" | "h3" | "h4" | "p" | "code" | "list" | "alert" | "quote" | "hr";
  content: string;
  lang?: string;
  lines: string[];
  items: { text: string; checked?: boolean }[];
  kind?: string;
  title?: string;
}

function parseMarkdown(text: string): Block[] {
  const lines = text.split("\n");
  const blocks: Block[] = [];
  let i = 0;

  while (i < lines.length) {
    const rawLine = lines[i]!;
    const line = rawLine.trimEnd();

    // Code block
    if (line.trimStart().startsWith("```")) {
      const lang = line.trimStart().slice(3).trim();
      const codeLines: string[] = [];
      i++;
      while (i < lines.length && !lines[i]!.trimStart().startsWith("```")) {
        codeLines.push(lines[i]!);
        i++;
      }
      i++; // skip closing ```
      blocks.push({
        type: "code",
        content: codeLines.join("\n"),
        lang,
        lines: codeLines,
        items: [],
      });
      continue;
    }

    // Horizontal Rule
    if (/^(\*{3,}|-{3,}|_{3,})$/.test(line.trim())) {
      blocks.push({ type: "hr", content: "", lines: [], items: [] });
      i++;
      continue;
    }

    // Alerts (> [!NOTE], > [!IMPORTANT], > [!TIP], > [!WARNING])
    const alertMatch = line.match(/^>\s*\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\](.*)$/i);
    if (alertMatch) {
      const kindStr = alertMatch[1]!.toLowerCase();
      const kind = kindStr === "note" || kindStr === "tip" ? "info" : kindStr === "important" ? "ok" : "warn";
      const title = alertMatch[2]!.trim() || alertMatch[1]!.toUpperCase();
      const alertLines: string[] = [];
      i++;
      while (i < lines.length && lines[i]!.trimStart().startsWith(">")) {
        alertLines.push(lines[i]!.trimStart().replace(/^>\s?/, ""));
        i++;
      }
      blocks.push({
        type: "alert",
        kind,
        title,
        content: alertLines.join(" "),
        lines: alertLines,
        items: [],
      });
      continue;
    }

    // Standard Blockquote
    if (line.startsWith(">")) {
      const quoteLines: string[] = [];
      while (i < lines.length && lines[i]!.trimStart().startsWith(">")) {
        quoteLines.push(lines[i]!.trimStart().replace(/^>\s?/, ""));
        i++;
      }
      blocks.push({
        type: "quote",
        content: quoteLines.join(" "),
        lines: quoteLines,
        items: [],
      });
      continue;
    }

    // Headers
    if (line.startsWith("# ")) {
      blocks.push({ type: "h1", content: line.slice(2).trim(), lines: [], items: [] });
      i++;
      continue;
    }
    if (line.startsWith("## ")) {
      blocks.push({ type: "h2", content: line.slice(3).trim(), lines: [], items: [] });
      i++;
      continue;
    }
    if (line.startsWith("### ")) {
      blocks.push({ type: "h3", content: line.slice(4).trim(), lines: [], items: [] });
      i++;
      continue;
    }
    if (line.startsWith("#### ")) {
      blocks.push({ type: "h4", content: line.slice(5).trim(), lines: [], items: [] });
      i++;
      continue;
    }

    // Lists
    if (/^(\s*[-*+]|\s*\d+\.)\s/.test(line)) {
      const items: { text: string; checked?: boolean }[] = [];
      while (i < lines.length && /^(\s*[-*+]|\s*\d+\.)\s/.test(lines[i]!)) {
        const itemLine = lines[i]!.replace(/^\s*[-*+]\s+/, "").replace(/^\s*\d+\.\s+/, "");
        const taskMatch = itemLine.match(/^\[([ xX])\]\s*(.*)$/);
        if (taskMatch) {
          items.push({ text: taskMatch[2]!, checked: taskMatch[1]!.toLowerCase() === "x" });
        } else {
          items.push({ text: itemLine });
        }
        i++;
      }
      blocks.push({ type: "list", content: "", lines: [], items });
      continue;
    }

    // Empty lines
    if (!line.trim()) {
      i++;
      continue;
    }

    // Paragraph
    const pLines: string[] = [];
    while (
      i < lines.length &&
      lines[i]!.trim() &&
      !lines[i]!.startsWith("#") &&
      !lines[i]!.startsWith(">") &&
      !lines[i]!.trimStart().startsWith("```") &&
      !/^(\s*[-*+]|\s*\d+\.)\s/.test(lines[i]!)
    ) {
      pLines.push(lines[i]!.trim());
      i++;
    }
    blocks.push({ type: "p", content: pLines.join(" "), lines: pLines, items: [] });
  }

  return blocks;
}

function renderInline(text: string) {
  // Simple regex parser for inline code `...`, bold **...**, and links [text](url)
  const parts: React.ReactNode[] = [];
  const regex = /(`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\))/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = regex.exec(text)) !== null) {
    if (match.index > lastIndex) {
      parts.push(text.slice(lastIndex, match.index));
    }
    const token = match[0];
    if (token.startsWith("`") && token.endsWith("`")) {
      parts.push(
        <code key={match.index} className="mono" style={{ background: "var(--code-bg)", padding: "0.1rem 0.3rem" }}>
          {token.slice(1, -1)}
        </code>,
      );
    } else if (token.startsWith("**") && token.endsWith("**")) {
      parts.push(<strong key={match.index}>{token.slice(2, -2)}</strong>);
    } else if (token.startsWith("[")) {
      const linkMatch = token.match(/^\[([^\]]+)\]\(([^)]+)\)$/);
      if (linkMatch) {
        parts.push(
          <a key={match.index} href={linkMatch[2]} target="_blank" rel="noreferrer" className="link">
            {linkMatch[1]}
          </a>,
        );
      }
    }
    lastIndex = match.index + token.length;
  }

  if (lastIndex < text.length) {
    parts.push(text.slice(lastIndex));
  }

  return parts.length ? parts : text;
}
