import { useState } from "react";
import { post } from "../api";
import type { Pending } from "../types";
import { AgentChip, ErrorLine } from "../util";

const LABEL: Record<string, string> = {
  approve: "Approve",
  deny: "Deny",
  continue: "Continue (compact)",
  fresh: "Stop; fresh session later",
  rollback: "Stop; roll back",
};

/** Approval gates: every prompt the backend would ask on a terminal. */
export function Approvals({
  pending,
  colors,
  onAnswered,
}: {
  pending: Pending[];
  colors: (n: string | null | undefined) => string;
  onAnswered: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  if (!pending.length) return null;
  const answer = async (id: number, choice: string) => {
    setBusy(id);
    try {
      await post("/api/approvals/answer", { id, choice });
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      onAnswered();
    }
  };
  return (
    <section className="card approvals" aria-live="assertive" aria-label="Awaiting your approval">
      <h2>Awaiting your approval ({pending.length})</h2>
      <ErrorLine error={error} />
      {pending.map((p) => (
        <article key={p.id} className="approval" data-testid={`approval-${p.kind}`}>
          <div className="row">
            <span className={`badge badge-${p.kind === "merge" ? "info" : "warn"}`}>{p.kind}</span>
            <AgentChip name={p.agent} color={colors(p.agent)} />
          </div>
          <p className="summary">{p.summary}</p>
          {p.rule && <p className="muted small">rule: {p.rule}</p>}
          {p.detail && (
            <details open={p.kind === "merge"}>
              <summary>Details</summary>
              <pre className="pre">{p.detail}</pre>
            </details>
          )}
          <div className="actions">
            {p.choices.map((c) => (
              <button
                key={c}
                className={c === "approve" || c === "continue" ? "btn btn-primary" : "btn"}
                disabled={busy === p.id}
                onClick={() => answer(p.id, c)}
              >
                {LABEL[c] ?? c}
              </button>
            ))}
          </div>
        </article>
      ))}
    </section>
  );
}
