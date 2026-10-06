import type { RosterRow } from "../types";
import { num, usd } from "../util";

const STATUS: Record<string, string> = {
  thinking: "thinking",
  waiting: "waiting",
  blocked: "blocked",
  done: "done",
  failed: "failed",
};

/** The agent roster (§12.2): each model, its role, task and live status. */
export function Roster({ roster, colors }: { roster: RosterRow[]; colors: (n: string) => string }) {
  return (
    <section className="card" aria-label="Agents">
      <h2>Agents</h2>
      {!roster.length && <p className="empty">No agents yet.</p>}
      <ul className="roster">
        {roster.map((r) => (
          <li key={r.name} className="agent" style={{ borderLeftColor: colors(r.name) }} data-testid={`agent-${r.name}`}>
            <div className="row">
              <strong style={{ color: colors(r.name) }}>{r.name}</strong>
              <span className={`status status-${r.status}`} data-testid="status">
                {STATUS[r.status] ?? r.status}
              </span>
            </div>
            <div className="small">
              {r.role || "—"} · <code>{r.model_id || "?"}</code>
            </div>
            <div className="small muted">family {r.family || "?"}</div>
            <div className="small">{r.detail}</div>
            {r.instruction && <div className="small task" title={r.instruction}>Task: {r.instruction}</div>}
            {r.claim_within.length > 0 && <div className="small muted">may change: {r.claim_within.join(", ")}</div>}
            <div className="small muted">
              {r.sessions} session{r.sessions === 1 ? "" : "s"} · {num(r.cost.tokens_in + r.cost.tokens_out)} tokens ·{" "}
              {usd(r.cost.spend_usd)}
            </div>
            {r.hub_status && r.hub_status !== "active" && <div className="small muted">hub: {r.hub_status}</div>}
          </li>
        ))}
      </ul>
    </section>
  );
}
