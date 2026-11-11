import { useEffect, useRef, useState } from "react";
import { post } from "../api";
import { Badge, Empty, ErrorLine } from "../util";

interface CommandResult {
  id: string;
  command: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  elapsed_ms: number;
  timestamp: string;
}

const PRESETS = [
  { label: "Run Tests (unittest)", program: "python", args: ["-m", "unittest", "discover", "-v"] },
  { label: "Compile & Lint (compileall)", program: "python", args: ["-m", "compileall", "-q", "."] },
  { label: "Git Status", program: "git", args: ["status", "--porcelain", "-b"] },
  { label: "Git Diff", program: "git", args: ["diff"] },
  { label: "Cargo Check", program: "cargo", args: ["check"] },
  { label: "Cargo Test", program: "cargo", args: ["test"] },
];

export function TerminalPanel({
  onAskAgent,
  pendingCommand,
}: {
  onAskAgent?: (prompt: string) => void;
  pendingCommand?: { program: string; args: string[] } | null;
}) {
  const [inputCmd, setInputCmd] = useState("");
  const [running, setRunning] = useState(false);
  const [history, setHistory] = useState<CommandResult[]>([]);
  const [activeResult, setActiveResult] = useState<CommandResult | null>(null);
  const [historyIndex, setHistoryIndex] = useState<number>(-1);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const executeCommand = async (program: string, args: string[]) => {
    setRunning(true);
    setError(null);
    const fullCmd = [program, ...args].join(" ");
    try {
      const res = await post<{
        ok: boolean;
        exit_code: number;
        stdout: string;
        stderr: string;
        elapsed_ms: number;
      }>("/api/command/run", { program, args });

      const item: CommandResult = {
        id: Math.random().toString(36).slice(2, 9),
        command: fullCmd,
        exit_code: res.exit_code,
        stdout: res.stdout,
        stderr: res.stderr,
        elapsed_ms: res.elapsed_ms,
        timestamp: new Date().toLocaleTimeString(),
      };
      setHistory((prev) => [item, ...prev]);
      setActiveResult(item);
      setHistoryIndex(-1);
    } catch (e) {
      setError(String(e));
    } finally {
      setRunning(false);
    }
  };

  useEffect(() => {
    if (pendingCommand) {
      void executeCommand(pendingCommand.program, pendingCommand.args);
    }
  }, [pendingCommand]);

  const handleCustomSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const parts = inputCmd.trim().split(/\s+/).filter(Boolean);
    if (!parts.length || running) return;
    const program = parts[0]!;
    const args = parts.slice(1);
    setInputCmd("");
    void executeCommand(program, args);
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowUp") {
      if (!history.length) return;
      e.preventDefault();
      const nextIdx = Math.min(historyIndex + 1, history.length - 1);
      setHistoryIndex(nextIdx);
      setInputCmd(history[nextIdx]?.command ?? "");
    } else if (e.key === "ArrowDown") {
      if (historyIndex <= 0) {
        setHistoryIndex(-1);
        setInputCmd("");
      } else {
        e.preventDefault();
        const nextIdx = historyIndex - 1;
        setHistoryIndex(nextIdx);
        setInputCmd(history[nextIdx]?.command ?? "");
      }
    }
  };

  const askAgentAboutOutput = () => {
    if (!activeResult) return;
    const prompt = `Inspect the output of \`${activeResult.command}\` (exit code ${activeResult.exit_code}):\n\n\`\`\`\n${
      activeResult.stderr || activeResult.stdout || "(no output)"
    }\n\`\`\`\nAnalyze any failures or next actions needed.`;
    onAskAgent?.(prompt);
  };

  const copyOutput = async () => {
    if (!activeResult) return;
    const text = [
      `$ ${activeResult.command}`,
      `Exit Code: ${activeResult.exit_code}`,
      activeResult.stderr ? `--- STDERR ---\n${activeResult.stderr}` : "",
      activeResult.stdout ? `--- STDOUT ---\n${activeResult.stdout}` : "",
    ]
      .filter(Boolean)
      .join("\n\n");

    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // fallback
    }
  };

  return (
    <div className="wb-term-panel">
      <ErrorLine error={error} />

      {/* TERMINAL HEADER & PRESET SHORTCUTS */}
      <div className="wb-toolbar">
        <div className="row">
          <strong>SANDBOXED WORKSPACE TASK RUNNER</strong>
          <span className="muted small mono">
            Runs strictly confined inside workspace via anchor_exec
          </span>
        </div>
        <div className="row small">
          {activeResult && (
            <>
              <button
                type="button"
                className="btn btn-small"
                onClick={copyOutput}
                title="Copy terminal command output to clipboard"
              >
                {copied ? "✓ Copied" : "Copy Output"}
              </button>
              <button
                type="button"
                className="btn btn-small btn-plan"
                onClick={askAgentAboutOutput}
              >
                Ask Agent about output ▸
              </button>
            </>
          )}
        </div>
      </div>

      <div className="wb-term-presets">
        <span className="mono small muted">QUICK PRESETS:</span>
        {PRESETS.map((p) => (
          <button
            key={p.label}
            type="button"
            className="btn btn-small"
            disabled={running}
            onClick={() => void executeCommand(p.program, p.args)}
          >
            {p.label}
          </button>
        ))}
      </div>

      {/* CLI INPUT BAR */}
      <form className="wb-term-form" onSubmit={handleCustomSubmit}>
        <span className="wb-term-prompt mono">$</span>
        <input
          ref={inputRef}
          type="text"
          className="wb-term-input mono"
          placeholder="Run command in workspace sandbox (e.g. python -m unittest, git status, cargo test)…"
          value={inputCmd}
          disabled={running}
          onChange={(e) => setInputCmd(e.target.value)}
          onKeyDown={handleKeyDown}
          aria-label="Terminal command"
        />
        <button
          type="submit"
          className="btn btn-primary btn-small"
          disabled={running || !inputCmd.trim()}
        >
          {running ? "Running…" : "Execute ⏎"}
        </button>
      </form>

      {/* TERMINAL SPLIT: HISTORY + CONSOLE OUTPUT */}
      <div className="wb-split" style={{ marginTop: "0.6rem" }}>
        <div className="wb-sidebar" style={{ maxWidth: "16rem" }}>
          <div className="wb-section-head">
            <span>RUN HISTORY</span>
            <div className="row">
              {history.length > 0 && (
                <button
                  type="button"
                  className="link small"
                  onClick={() => {
                    setHistory([]);
                    setActiveResult(null);
                  }}
                  title="Clear history"
                >
                  Clear
                </button>
              )}
              <span className="mono muted">{history.length}</span>
            </div>
          </div>
          {!history.length && <Empty>No commands executed yet.</Empty>}
          <ul className="wb-filelist">
            {history.map((h) => (
              <li key={h.id}>
                <button
                  type="button"
                  className={activeResult?.id === h.id ? "wb-filebtn wb-filebtn-on" : "wb-filebtn"}
                  onClick={() => setActiveResult(h)}
                >
                  <div className="row" style={{ justifyContent: "space-between" }}>
                    <code
                      className="small"
                      style={{
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                        maxWidth: "9rem",
                      }}
                    >
                      {h.command}
                    </code>
                    <Badge kind={h.exit_code === 0 ? "ok" : "fail"}>
                      {h.exit_code === 0 ? "0" : `${h.exit_code}`}
                    </Badge>
                  </div>
                  <div className="row mono muted" style={{ fontSize: "0.68rem" }}>
                    <span>{h.timestamp}</span>
                    <span>·</span>
                    <span>{h.elapsed_ms}ms</span>
                  </div>
                </button>
              </li>
            ))}
          </ul>
        </div>

        <div className="wb-editor-pane">
          {!activeResult ? (
            <Empty>Select a past command or execute one above to view stdout and stderr.</Empty>
          ) : (
            <div className="wb-term-console">
              <div className="wb-toolbar">
                <div className="row">
                  <span className="mono bold">$ {activeResult.command}</span>
                  <Badge kind={activeResult.exit_code === 0 ? "ok" : "fail"}>
                    exit code: {activeResult.exit_code}
                  </Badge>
                </div>
                <div className="row mono small muted">
                  <span>{activeResult.elapsed_ms} ms</span>
                  <span>·</span>
                  <span>{activeResult.timestamp}</span>
                </div>
              </div>

              {activeResult.stderr && (
                <div style={{ marginTop: "0.4rem" }}>
                  <div className="wb-section-head" style={{ color: "var(--fail)" }}>
                    <span>STDERR</span>
                  </div>
                  <pre className="pre wb-term-output wb-term-stderr mono">
                    {activeResult.stderr}
                  </pre>
                </div>
              )}

              <div style={{ marginTop: "0.4rem" }}>
                <div className="wb-section-head">
                  <span>STDOUT</span>
                </div>
                <pre className="pre wb-term-output mono">
                  {activeResult.stdout || "(no standard output)"}
                </pre>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
