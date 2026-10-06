import { useEffect, useMemo, useState } from "react";
import { get, post } from "../api";
import type { Decision, Overview, Task } from "../types";
import { AgentChip, Badge, Empty, ErrorLine, dateTime } from "../util";

/** The decision log (§12.2): every architectural decision, who proposed
 * it, its rationale and evidence; filterable by agent, task and time. */
export function Decisions({
  version,
  colors,
  overview,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  overview: Overview | null;
}) {
  const [decisions, setDecisions] = useState<Decision[]>([]);
  const [tasks, setTasks] = useState<Task[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [agent, setAgent] = useState("");
  const [task, setTask] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [tick, setTick] = useState(0);
  const [reason, setReason] = useState<Record<number, string>>({});
  useEffect(() => {
    Promise.all([get<Decision[]>("/api/decisions"), get<Task[]>("/api/tasks")]).then(
      ([d, t]) => {
        setDecisions(d);
        setTasks(t);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version, tick]);
  // A task selects the agent working on it: each team agent's own task,
  // or a Hub task assigned to an agent.
  const taskOptions = useMemo(() => {
    const opts: { key: string; label: string; agent: string | null }[] = [];
    for (const r of overview?.roster ?? []) {
      if (r.instruction) opts.push({ key: `team:${r.name}`, label: `${r.name}: ${r.instruction}`, agent: r.name });
    }
    for (const t of tasks) {
      const a = overview?.roster.find((r) => r.agent_id === t.assigned_to_agent_id)?.name ?? null;
      opts.push({ key: `hub:${t.id}`, label: `task #${t.id}: ${t.description}`, agent: a });
    }
    return opts;
  }, [overview, tasks]);
  const taskAgent = taskOptions.find((o) => o.key === task)?.agent;
  const fromMs = from ? new Date(from).getTime() : null;
  const toMs = to ? new Date(to).getTime() : null;
  const shown = decisions.filter(
    (d) =>
      (!agent || d.agent === agent) &&
      (!task || d.agent === taskAgent) &&
      (fromMs === null || d.created_at >= fromMs) &&
      (toMs === null || d.created_at <= toMs),
  );
  const decide = async (id: number, accept: boolean) => {
    try {
      await post("/api/decisions/decide", { id, accept, reason: reason[id] ?? "" });
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    setTick((t) => t + 1);
  };
  return (
    <div>
      <ErrorLine error={error} />
      <div className="filters">
        <label>
          Agent{" "}
          <select value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">all</option>
            {(overview?.roster ?? []).map((r) => (
              <option key={r.name}>{r.name}</option>
            ))}
          </select>
        </label>
        <label>
          Task{" "}
          <select value={task} onChange={(e) => setTask(e.target.value)} className="wide-select">
            <option value="">all</option>
            {taskOptions.map((o) => (
              <option key={o.key} value={o.key}>
                {o.label.length > 70 ? `${o.label.slice(0, 70)}…` : o.label}
              </option>
            ))}
          </select>
        </label>
        <label>
          From <input type="datetime-local" value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label>
          To <input type="datetime-local" value={to} onChange={(e) => setTo(e.target.value)} />
        </label>
      </div>
      {!shown.length && <Empty>No decisions{decisions.length ? " match the filters" : " yet"}.</Empty>}
      <ol className="list">
        {shown.map((d) => (
          <li key={d.id} className="decision" data-testid={`decision-${d.id}`}>
            <div className="row">
              <strong>#{d.id}</strong>
              <AgentChip name={d.agent} color={colors(d.agent)} />
              <Badge kind={d.status === "accepted" ? "ok" : d.status === "rejected" ? "fail" : "warn"}>{d.status}</Badge>
              <span className="muted small">{dateTime(d.created_at)}</span>
            </div>
            <p className="summary">{d.summary}</p>
            <p className="small">
              <b>Why:</b> {d.rationale}
            </p>
            <p className="small muted">
              Evidence: {d.evidence_ref !== null ? `artifact #${d.evidence_ref}` : "none cited"}
              {d.decided_reason ? ` · your reason: ${d.decided_reason}` : ""}
            </p>
            {d.status === "proposed" && (
              <div className="actions">
                <input
                  placeholder="reason (optional)"
                  value={reason[d.id] ?? ""}
                  maxLength={2000}
                  onChange={(e) => setReason({ ...reason, [d.id]: e.target.value })}
                  aria-label="Reason"
                />
                <button className="btn btn-primary" onClick={() => decide(d.id, true)}>
                  Accept
                </button>
                <button className="btn" onClick={() => decide(d.id, false)}>
                  Reject
                </button>
              </div>
            )}
          </li>
        ))}
      </ol>
    </div>
  );
}
