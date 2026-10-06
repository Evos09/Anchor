import { useState } from "react";
import type { Event, Overview } from "../types";
import { show, time } from "../util";

/** The raw audit log, as verified: every entry, nothing summarized. */
export function Audit({ events, overview }: { events: Event[]; overview: Overview | null }) {
  const [kind, setKind] = useState("");
  const kinds = [...new Set(events.map((e) => e.kind))].sort();
  const shown = events.filter((e) => !kind || e.kind === kind);
  return (
    <div>
      <p className={overview?.chain.error ? "fail" : "ok"}>
        {overview?.chain.error
          ? `The hash chain is broken: ${overview.chain.error}`
          : `✔ ${overview?.chain.verified_entries ?? 0} entries, each verified to extend the hash chain.`}
      </p>
      <div className="filters">
        <label>
          Kind{" "}
          <select value={kind} onChange={(e) => setKind(e.target.value)}>
            <option value="">all ({events.length})</option>
            {kinds.map((k) => (
              <option key={k}>{k}</option>
            ))}
          </select>
        </label>
      </div>
      <table className="table small">
        <thead>
          <tr>
            <th>seq</th>
            <th>time</th>
            <th>kind</th>
            <th>agent</th>
            <th>data</th>
          </tr>
        </thead>
        <tbody>
          {shown.map((e) => (
            <tr key={e.seq}>
              <td>{e.seq}</td>
              <td>{time(e.ts_ms)}</td>
              <td>
                <code>{e.kind}</code>
              </td>
              <td>{e.agent ?? ""}</td>
              <td>
                <details>
                  <summary>{e.text}</summary>
                  <pre className="pre">{show(e.data)}</pre>
                </details>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
