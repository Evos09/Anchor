import { useEffect, useState } from "react";
import { get } from "../api";
import type { CostView, Tally } from "../types";
import { AgentChip, Empty, ErrorLine, num, usd } from "../util";

/** The cost meter (§12.2): tokens and spend per agent, per session and
 * per model, and the project total. Every reported cost is recomputed
 * from the prices in your roles files; disagreements are shown, never
 * smoothed over. */
export function Cost({ version, colors }: { version: number; colors: (n: string | null | undefined) => string }) {
  const [v, setV] = useState<CostView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lines, setLines] = useState(false);
  useEffect(() => {
    get<CostView>("/api/cost").then(
      (x) => {
        setV(x);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version]);
  if (!v) return <ErrorLine error={error} />;
  const c = v.cost;
  const table = (title: string, rows: Record<string, Tally>, chip: boolean, testid: string) => (
    <div>
      <h3>{title}</h3>
      {!Object.keys(rows).length && <Empty>No model calls yet.</Empty>}
      <table className="table num" data-testid={testid}>
        <thead>
          <tr>
            <th />
            <th>calls</th>
            <th>tokens in</th>
            <th>tokens out</th>
            <th>spend</th>
            <th>runtime reported</th>
          </tr>
        </thead>
        <tbody>
          {Object.entries(rows).map(([k, t]) => (
            <tr key={k} data-testid={`${testid}-${k}`}>
              <td>{chip ? <AgentChip name={k} color={colors(k)} /> : <code className="small">{k}</code>}</td>
              <td>{t.calls}</td>
              <td>{num(t.tokens_in)}</td>
              <td>{num(t.tokens_out)}</td>
              <td data-testid="spend">
                <b>{usd(t.spend_usd, 6)}</b>
                {t.unpriced_calls > 0 && <span className="warn small"> ({t.unpriced_calls} unpriced)</span>}
              </td>
              <td className="muted">{usd(t.reported_usd, 6)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
  return (
    <div>
      <ErrorLine error={error} />
      <div className="total" data-testid="cost-total">
        <span className="big">{usd(c.total.spend_usd, 6)}</span>
        <span>
          {num(c.total.tokens_in)} tokens in · {num(c.total.tokens_out)} tokens out · {c.total.calls} model calls
        </span>
      </div>
      <p className="small muted">How it is computed: {v.formula}.</p>
      {c.discrepancies.length > 0 ? (
        <div className="banner banner-fail" role="alert" data-testid="discrepancies">
          <b>Discrepancies</b>
          <ul>
            {c.discrepancies.map((d, i) => (
              <li key={i}>{d}</li>
            ))}
          </ul>
        </div>
      ) : (
        <p className="ok small" data-testid="no-discrepancies">
          ✔ Every reported cost matches your prices, and every session's own total matches its calls.
        </p>
      )}
      {table("Per agent", c.per_agent, true, "cost-agent")}
      {table("Per model", c.per_model, false, "cost-model")}
      {table("Per session", c.per_session, false, "cost-session")}
      <label>
        <input type="checkbox" checked={lines} onChange={(e) => setLines(e.target.checked)} /> show every model call
      </label>
      {lines && (
        <table className="table num small">
          <thead>
            <tr>
              <th>#</th>
              <th>agent</th>
              <th>role</th>
              <th>model</th>
              <th>in</th>
              <th>out</th>
              <th>price in/out per Mtok</th>
              <th>computed</th>
              <th>reported</th>
            </tr>
          </thead>
          <tbody>
            {c.lines.map((l) => (
              <tr key={l.seq} className={l.computed_usd !== null && Math.abs(l.computed_usd - l.reported_usd) > 1e-9 ? "row-fail" : ""}>
                <td>{l.seq}</td>
                <td>{l.agent}</td>
                <td>{l.role}</td>
                <td>
                  <code>{l.model_id}</code>
                </td>
                <td>{num(l.tokens_in)}</td>
                <td>{num(l.tokens_out)}</td>
                <td>{l.price ? `${usd(l.price[0], 2)} / ${usd(l.price[1], 2)}` : "unknown"}</td>
                <td>{usd(l.computed_usd, 6)}</td>
                <td>{usd(l.reported_usd, 6)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
