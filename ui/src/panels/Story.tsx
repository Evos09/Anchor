import { useMemo, useState } from "react";
import type { Tab } from "../App";
import type { Event } from "../types";
import { AgentChip, Badge, show, time } from "../util";

const ICON: Record<string, string> = { fail: "✖", warn: "▲", ok: "✔", info: "•", trace: "·" };

/** "What happened": Antigravity Mission Control & narrated event stream.
 * Bookkeeping is folded away by default; failures never are. */
export function Story({
  events,
  colors,
  agents,
  onMerge,
  onOpenTab,
  onInspectFile,
}: {
  events: Event[];
  colors: (n: string | null | undefined) => string;
  agents: string[];
  onMerge: (seq: number) => void;
  onOpenTab?: (t: Tab) => void;
  onInspectFile?: (path: string) => void;
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
    <div className="story-stage">
      {/* Antigravity Mission Control Deck */}
      <div className="mc-deck">
        <div className="mc-card">
          <div className="wb-section-head">
            <span>CODE WORKBENCH & GUARD</span>
            <Badge kind="info">READ-BEFORE-WRITE</Badge>
          </div>
          <p className="small muted mc-desc">
            Inspect workspace files with line numbers, edit in-place, and commit audited mutations with automatic SHA-256
            receipts.
          </p>
          <div className="actions">
            <button
              type="button"
              className="btn btn-primary btn-small"
              onClick={() => {
                onInspectFile?.("calc.py");
                onOpenTab?.("workbench");
              }}
            >
              Open Code Workbench ▸
            </button>
            <button type="button" className="btn btn-small" onClick={() => onOpenTab?.("claims")}>
              File Claim Map
            </button>
          </div>
        </div>

        <div className="mc-card">
          <div className="wb-section-head">
            <span>ARTIFACTS & PLANS STUDIO</span>
            <Badge kind="ok">R1 · COT</Badge>
          </div>
          <p className="small muted mc-desc">
            Author and review structured implementation plans, architecture specs, and verification walkthroughs.
          </p>
          <div className="actions">
            <button type="button" className="btn btn-primary btn-small" onClick={() => onOpenTab?.("artifacts")}>
              Open Artifacts Studio ▸
            </button>
            <button type="button" className="btn btn-small" onClick={() => onOpenTab?.("decisions")}>
              Decision Log
            </button>
          </div>
        </div>

        <div className="mc-card">
          <div className="wb-section-head">
            <span>SKILLS, HOOKS & SANDBOX</span>
            <Badge kind="ok">CORDIS L1/L2</Badge>
          </div>
          <p className="small muted mc-desc">
            Inspect active ANCHOR.md workspace rules, progressive SKILL.md capabilities, and PreToolUse / PostToolUse
            hooks.
          </p>
          <div className="actions">
            <button type="button" className="btn btn-primary btn-small" onClick={() => onOpenTab?.("customizations")}>
              Skills & Hooks ▸
            </button>
            <button type="button" className="btn btn-small" onClick={() => onOpenTab?.("snapshots")}>
              Snapshots & Rollback
            </button>
          </div>
        </div>
      </div>

      <div className="filters">
        <strong className="mono small" style={{ letterSpacing: "0.04em" }}>
          MISSION TELEMETRY STREAM
        </strong>
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
          <input type="checkbox" checked={failuresOnly} onChange={(e) => setFailuresOnly(e.target.checked)} /> problems
          only
        </label>
        <label>
          <input type="checkbox" checked={showTrace} onChange={(e) => setShowTrace(e.target.checked)} /> show
          bookkeeping
        </label>
        <input
          type="search"
          placeholder="Filter trajectory…"
          value={text}
          onChange={(e) => setText(e.target.value)}
          aria-label="Search events"
        />
        <span className="muted small mono">
          {shown.length} shown{hidden ? `, ${hidden} filtered` : ""}
        </span>
      </div>

      {!shown.length && (
        <div className="mc-ready-banner">
          <div className="row">
            <span className="status-led status-led-on" aria-hidden="true" />
            <strong>ANTIGRAVITY HARNESS READY — AWAITING MISSION DISPATCH</strong>
          </div>
          <p className="small muted" style={{ margin: "0.35rem 0 0" }}>
            Click any <strong>Quick-Launch Mission</strong> in the right-hand <strong>Agent Sidecar</strong>, open a file
            from the left <strong>Workspace Explorer</strong>, or dispatch a prompt below. Every reasoning block, file
            mutation, and verification gate will stream here in real time.
          </p>
        </div>
      )}

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
