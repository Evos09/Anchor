import { useState } from "react";
import { post } from "../api";
import type { Tab } from "../App";
import type { Event, Overview } from "../types";
import { AgentChip, Badge, time } from "../util";
import type { ExecMode } from "./CommandBar";

const QUICK_MISSIONS: { title: string; mode: ExecMode; prompt: string; tag: string }[] = [
  {
    tag: "PLAN · R1",
    title: "Draft Architecture & Implementation Plan",
    mode: "plan",
    prompt: "Analyze all workspace files and ANCHOR.md rules, then author a step-by-step implementation_plan.md with verification criteria.",
  },
  {
    tag: "AUDIT",
    title: "Verify Read-Before-Write & Append-Only Guards",
    mode: "supervised",
    prompt: "Inspect workspace files for append-only invariants, boundary conditions, and test coverage gaps.",
  },
  {
    tag: "TEAM",
    title: "Prepare Multi-Agent File Claim Partition",
    mode: "team",
    prompt: "Partition workspace files into non-overlapping edit claims for parallel executor agents and a cross-family reviewer.",
  },
];

export function AgentSidecar({
  overview,
  events,
  selectedFile,
  colors,
  onDispatched,
  onOpenTab,
}: {
  overview: Overview | null;
  events: Event[];
  selectedFile: string | null;
  colors: (n: string | null | undefined) => string;
  onDispatched: () => Promise<void>;
  onOpenTab: (t: Tab) => void;
}) {
  const [prompt, setPrompt] = useState("");
  const [mode, setMode] = useState<ExecMode>("plan");
  const [busy, setBusy] = useState(false);
  const [lastError, setLastError] = useState<string | null>(null);

  const runMission = async (text: string, execMode: ExecMode) => {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    setLastError(null);
    try {
      await post("/api/session/prompt", { prompt: trimmed, mode: execMode });
      setPrompt("");
      await onDispatched();
    } catch (e) {
      setLastError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const agentEvents = events.filter(
    (e) =>
      e.kind === "reasoning_step" ||
      e.kind === "artifact_written" ||
      e.kind === "todos_updated" ||
      e.kind === "ui_action" ||
      e.kind === "mutation" ||
      e.kind === "command" ||
      e.kind === "session_started" ||
      e.kind === "session_finished" ||
      e.level === "fail" ||
      e.level === "warn",
  );

  const todos = overview?.todos ?? [];

  return (
    <aside className="agent-dock" aria-label="Antigravity Agent Sidecar">
      <div className="dock-header">
        <div className="row">
          <span className="status-led status-led-on" aria-hidden="true" />
          <strong className="mono">AGENT SIDECAR</strong>
          <Badge kind="info">CORDIS · L2</Badge>
        </div>
        <div className="row mono small muted">
          <span>{overview?.reasoning_steps ?? 0} CoT</span>
          <span>·</span>
          <span>{todos.length} tasks</span>
        </div>
      </div>

      <div className="dock-stream">
        <div className="dock-welcome">
          <div className="wb-section-head">
            <span>QUICK-LAUNCH MISSIONS</span>
            <span className="mono muted">1-CLICK</span>
          </div>
          <div className="mission-grid">
            {QUICK_MISSIONS.map((m) => (
              <button
                key={m.title}
                type="button"
                className="mission-card"
                disabled={busy}
                onClick={() => void runMission(m.prompt, m.mode)}
              >
                <div className="row">
                  <span className="mission-tag">{m.tag}</span>
                  <strong className="small">{m.title}</strong>
                </div>
                <div className="small muted mission-desc">{m.prompt}</div>
              </button>
            ))}
          </div>
        </div>

        {todos.length > 0 && (
          <div className="dock-card">
            <div className="wb-section-head">
              <span>ACTIVE TASK CHECKLIST (TODOWRITE)</span>
              <span className="mono">
                {todos.filter((t) => t.status === "completed").length}/{todos.length}
              </span>
            </div>
            <ul className="todo-list">
              {todos.map((t) => (
                <li key={t.id} className={`todo-item todo-${t.status}`}>
                  <span className="todo-box">
                    {t.status === "completed" ? "■" : t.status === "in_progress" ? "▸" : "□"}
                  </span>
                  <span>{t.content}</span>
                </li>
              ))}
            </ul>
          </div>
        )}

        <div className="wb-section-head" style={{ marginTop: "0.6rem" }}>
          <span>LIVE TRAJECTORY & REASONING</span>
          <span className="mono muted">{agentEvents.length} steps</span>
        </div>

        {!agentEvents.length ? (
          <div className="dock-empty small muted">
            Select a Quick-Launch Mission above or type an instruction below to start an audited reasoning & execution
            turn.
          </div>
        ) : (
          <ol className="dock-trajectory">
            {agentEvents.slice(-30).map((e) => {
              const isReasoning = e.kind === "reasoning_step";
              const isArtifact = e.kind === "artifact_written";
              return (
                <li
                  key={e.seq}
                  className={`traj-item ${isReasoning ? "traj-reasoning" : ""} ${isArtifact ? "traj-artifact" : ""}`}
                >
                  <div className="row traj-meta">
                    <span className="mono muted small">{time(e.ts_ms)}</span>
                    <AgentChip name={e.agent} color={colors(e.agent)} />
                    <span className="mono small muted">{e.kind}</span>
                  </div>
                  <div className="traj-text">{e.text}</div>
                  {isArtifact && (
                    <div className="actions">
                      <button
                        type="button"
                        className="btn btn-small btn-primary"
                        onClick={() => onOpenTab("artifacts")}
                      >
                        Open Artifact in Studio ▸
                      </button>
                    </div>
                  )}
                </li>
              );
            })}
          </ol>
        )}
      </div>

      <form
        className="dock-composer"
        onSubmit={(e) => {
          e.preventDefault();
          void runMission(prompt, mode);
        }}
      >
        {lastError && (
          <div className="banner banner-fail small" role="alert">
            {lastError}
          </div>
        )}
        <div className="composer-context">
          <span className="ctx-pill">@workspace</span>
          {selectedFile && <span className="ctx-pill">@{selectedFile}</span>}
          <span className="ctx-pill">guard:on</span>
          <span className="ctx-pill">sandbox:on</span>
        </div>
        <textarea
          className="composer-input"
          rows={3}
          placeholder="Ask Anchor Agent to plan, edit, verify, or audit… (Enter to dispatch, Shift+Enter for newline)"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void runMission(prompt, mode);
            }
          }}
          aria-label="Agent Mission Prompt"
        />
        <div className="composer-footer">
          <div className="cmd-modes" role="group" aria-label="Agent Mode">
            {(
              [
                ["plan", "PLAN · R1"],
                ["supervised", "SUPERVISED"],
                ["sandbox-auto", "AUTO"],
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
          <button type="submit" className="btn btn-primary btn-send" disabled={busy || !prompt.trim()}>
            {busy ? "Running…" : "Dispatch ⏎"}
          </button>
        </div>
      </form>
    </aside>
  );
}
