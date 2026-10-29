import { useState } from "react";
import { post } from "../api";
import type { Tab } from "../App";
import type { Overview } from "../types";

const HELP: [string, string][] = [
  ["/agents", "show the agent roster and claim map"],
  ["/cost", "show the cost meter"],
  ["/rollback REF", "roll back to a snapshot (preview first)"],
  ["/code", "open the Code Workbench & file inspector"],
  ["/artifacts", "open Artifacts & Implementation Plans"],
  ["/skills", "inspect rules, skills, subagents, hooks & MCP"],
  ["/plan", "start a planning session"],
  ["/implement", "start an implementation session against a plan"],
  ["/review", "start a review session against a diff"],
];

export type ExecMode = "plan" | "supervised" | "sandbox-auto" | "team";

/** Slash commands (§12.4) & Omni-Prompt Dock. Those that start CLI sessions
 * print the exact command or queue an audited session prompt. */
export function CommandBar({
  overview,
  onTab,
  onRollback,
  onToggleWorkbench,
  workbenchOpen,
}: {
  overview: Overview | null;
  onTab: (t: Tab) => void;
  onRollback: (ref: string) => void;
  onToggleWorkbench?: () => void;
  workbenchOpen?: boolean;
}) {
  const [text, setText] = useState("");
  const [mode, setMode] = useState<ExecMode>("supervised");
  const [out, setOut] = useState<string | null>(null);
  const ws = overview?.workspace ?? "DIR";

  const run = async () => {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (!trimmed.startsWith("/")) {
      setText("");
      try {
        await post("/api/session/prompt", { prompt: trimmed, mode });
        setOut(`[${mode.toUpperCase()}] Queued audited instruction: "${trimmed}"`);
      } catch (e) {
        setOut(`Failed to queue instruction: ${String(e)}`);
      }
      return;
    }

    const [cmd, ...rest] = trimmed.split(/\s+/);
    const arg = rest.join(" ");
    switch (cmd) {
      case "/agents":
        onTab("claims");
        setOut("The roster is on the left; the claim map is open.");
        break;
      case "/cost":
        onTab("cost");
        setOut(null);
        break;
      case "/code":
        onTab("workbench");
        setOut(null);
        break;
      case "/artifacts":
        onTab("artifacts");
        setOut(null);
        break;
      case "/skills":
      case "/hooks":
      case "/mcp":
        onTab("customizations");
        setOut(null);
        break;
      case "/rollback":
        if (!arg) {
          onTab("snapshots");
          setOut("Pick a snapshot to restore, or type /rollback REF.");
        } else {
          onRollback(arg);
          setOut(null);
        }
        break;
      case "/plan":
        setOut(`In a terminal: anchor run --workspace ${ws} --roles ROLES.toml --task "plan: …"`);
        break;
      case "/implement":
        setOut(`In a terminal: anchor team run --workspace ${ws} --team TEAM.toml --ui`);
        break;
      case "/review":
        setOut(
          `Verification reviews every merge (team.toml [verify]). One-off: anchor eval run --corpus DIR --reviewer model …`,
        );
        break;
      default:
        setOut(HELP.map(([c, d]) => `${c.padEnd(14)} ${d}`).join("\n"));
    }
    setText("");
  };

  return (
    <div className="cmdbar">
      <form
        className="cmdbar-form"
        onSubmit={(e) => {
          e.preventDefault();
          void run();
        }}
      >
        <div className="cmd-modes" role="group" aria-label="Execution mode">
          {(
            [
              ["plan", "PLAN · R1"],
              ["supervised", "SUPERVISED"],
              ["sandbox-auto", "SANDBOX AUTO"],
              ["team", "TEAM"],
            ] as const
          ).map(([m, label]) => (
            <button
              key={m}
              type="button"
              className={mode === m ? "mode-pill mode-pill-on" : "mode-pill"}
              onClick={() => setMode(m)}
            >
              {label}
            </button>
          ))}
        </div>
        <input
          aria-label="Command"
          placeholder="/agents  /cost  /rollback REF  /code  /artifacts  /skills  /plan  /implement  /review  or prompt (Enter)"
          value={text}
          onChange={(e) => setText(e.target.value)}
          list="slash-commands"
        />
        {onToggleWorkbench && (
          <button
            type="button"
            className={workbenchOpen ? "btn btn-small btn-primary" : "btn btn-small"}
            onClick={onToggleWorkbench}
            title="Toggle side-by-side Code Workbench inspector"
          >
            {workbenchOpen ? "Hide Inspector" : "Split Inspector"}
          </button>
        )}
        <datalist id="slash-commands">
          {HELP.map(([c]) => (
            <option key={c} value={c.split(" ")[0]} />
          ))}
        </datalist>
      </form>
      {out && (
        <pre className="pre cmd-out" role="status">
          {out}
        </pre>
      )}
    </div>
  );
}
