import { useMemo, useState } from "react";
import type { Event } from "../types";
import { AgentChip, Empty, show, time } from "../util";

const ICON: Record<string, string> = { fail: "✖", warn: "▲", ok: "✔", info: "•", trace: "·" };

/** "What happened": the whole collaboration, one sentence per audited
 * event, in order. Bookkeeping is folded away by default; failures never
 * are. */
export function Story({
  events,
  colors,
  agents,
  onMerge,
}: {
  events: Event[];
  colors: (n: string | null | undefined) => string;
  agents: string[];
  onMerge: (seq: number) => void;
}) {
  const [agent, setAgent] = useState("");
  const [category, setCategory] = useState("");
  const [failuresOnly, setFailuresOnly] = useState(false);
  const [showTrace, setShowTrace] = useState(false);
  const [text, setText] = useState("");
  const categories = useMemo(() => [...new Set(events.map((e) => e.category))].sort(), [events]);
  const shown = useMemo(
    () =>
      events.filter(
        (e) =>
          (showTrace || e.level !== "trace") &&
          (!failuresOnly || e.level === "fail" || e.level === "warn") &&
          (!agent || e.agent === agent) &&
          (!category || e.category === category) &&
          (!text || e.text.toLowerCase().includes(text.toLowerCase())),
      ),
    [events, agent, category, failuresOnly, showTrace, text],
  );
  const hidden = events.length - shown.length;
  return (
    <div>
      <div className="filters">
        <label>
          Agent{" "}
          <select value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">all</option>
            {agents.map((a) => (
              <option key={a}>{a}</option>
            ))}
          </select>
        </label>
        <label>
          Kind{" "}
          <select value={category} onChange={(e) => setCategory(e.target.value)}>
            <option value="">all</option>
            {categories.map((c) => (
              <option key={c}>{c}</option>
            ))}
          </select>
        </label>
        <label>
          <input type="checkbox" checked={failuresOnly} onChange={(e) => setFailuresOnly(e.target.checked)} /> problems only
        </label>
        <label>
          <input type="checkbox" checked={showTrace} onChange={(e) => setShowTrace(e.target.checked)} /> show bookkeeping
        </label>
        <input type="search" placeholder="search" value={text} onChange={(e) => setText(e.target.value)} aria-label="Search events" />
        <span className="muted small">
          {shown.length} shown{hidden ? `, ${hidden} filtered` : ""}
        </span>
      </div>
      {!shown.length && <Empty>Nothing yet. Events appear here as the agents work.</Empty>}
      <ol className="story" data-testid="story">
        {shown.map((e) => {
          const merge = mergeOf(e);
          return (
            <li key={e.seq} className={`ev ev-${e.level}`}>
              <span className="ev-time">{time(e.ts_ms)}</span>
              <span className={`ev-icon lvl-${e.level}`} aria-label={e.level}>
                {ICON[e.level]}
              </span>
              <AgentChip name={e.agent} color={colors(e.agent)} />
              <details className="ev-body">
                <summary>
                  {e.text}
                  {merge !== null && (
                    <button
                      className="link small"
                      onClick={(ev) => {
                        ev.preventDefault();
                        onMerge(merge);
                      }}
                    >
                      evidence
                    </button>
                  )}
                </summary>
                <div className="small muted">
                  #{e.seq} · {e.kind}
                  {e.session_id ? ` · session ${e.session_id}` : ""}
                </div>
                <pre className="pre">{show(e.data)}</pre>
              </details>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function mergeOf(e: Event): number | null {
  if (!e.kind.startsWith("merge_")) return null;
  const d = e.data as { seq?: unknown };
  return typeof d?.seq === "number" ? d.seq : null;
}
