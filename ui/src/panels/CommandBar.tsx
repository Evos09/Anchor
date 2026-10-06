import { useState } from "react";
import type { Tab } from "../App";
import type { Overview } from "../types";

const HELP: [string, string][] = [
  ["/agents", "show the agent roster and claim map"],
  ["/cost", "show the cost meter"],
  ["/rollback REF", "roll back to a snapshot (preview first)"],
  ["/plan", "start a planning session"],
  ["/implement", "start an implementation session against a plan"],
  ["/review", "start a review session against a diff"],
];

/** Slash commands (§12.4). Those that start sessions print the CLI
 * command: starting sessions from the dashboard comes with the editor. */
export function CommandBar({
  overview,
  onTab,
  onRollback,
}: {
  overview: Overview | null;
  onTab: (t: Tab) => void;
  onRollback: (ref: string) => void;
}) {
  const [text, setText] = useState("");
  const [out, setOut] = useState<string | null>(null);
  const ws = overview?.workspace ?? "DIR";
  const run = () => {
    const [cmd, ...rest] = text.trim().split(/\s+/);
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
        setOut(`Verification reviews every merge (team.toml [verify]). One-off: anchor eval run --corpus DIR --reviewer model …`);
        break;
      default:
        setOut(HELP.map(([c, d]) => `${c.padEnd(14)} ${d}`).join("\n"));
    }
    setText("");
  };
  return (
    <div className="cmdbar">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          run();
        }}
      >
        <input
          aria-label="Command"
          placeholder="/agents  /cost  /rollback REF  /plan  /implement  /review  (Enter)"
          value={text}
          onChange={(e) => setText(e.target.value)}
          list="slash-commands"
        />
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
