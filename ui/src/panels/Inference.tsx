// The three ways a task can run (Update #1): a local model through Ollama,
// the user's own API key, or the free tier. These pieces show where a task
// will run, what to do when nothing can run it, the free tier's meters, and
// what happens when a task goes wrong.

import { useEffect, useState } from "react";
import { get } from "../api";
import type { InferenceStatus, InfPath, Meter, TaskPrompt } from "../types";

export const PATH_NAME: Record<InfPath, string> = {
  A: "Free tier",
  B: "Your API key",
  C: "Local",
};

/** The inference status, refreshed while shown (it probes Ollama). */
export function useInference(every = 15000): [InferenceStatus | null, () => void, string | null] {
  const [s, setS] = useState<InferenceStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let live = true;
    get<InferenceStatus>("/api/app/inference").then(
      (v) => live && (setS(v), setError(null)),
      (e) => live && setError(String(e)),
    );
    const t = window.setInterval(() => setTick((n) => n + 1), every);
    return () => {
      live = false;
      window.clearInterval(t);
    };
  }, [tick, every]);
  return [s, () => setTick((n) => n + 1), error];
}

function when(ms: number | null): string {
  if (ms === null) return "starts with your next request";
  return `resets ${new Date(ms).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit", month: "short", day: "numeric" })}`;
}

function Bar({ label, m }: { label: string; m: Meter }) {
  const pct = m.budget > 0 ? Math.min(100, (100 * m.used) / m.budget) : 0;
  const level = pct >= 100 ? "fail" : pct >= 80 ? "warn" : "ok";
  return (
    <div className="meter" data-testid={`meter-${label}`}>
      <div className="meter-head">
        <span>{label}</span>
        <span className="muted small">
          {m.used.toLocaleString()} / {m.budget.toLocaleString()} · {when(m.resets_at_ms)}
        </span>
      </div>
      <div
        className="meter-track"
        role="meter"
        aria-label={`${label} free-tier use`}
        aria-valuemin={0}
        aria-valuemax={m.budget}
        aria-valuenow={m.used}
      >
        <div className={`meter-fill meter-${level}`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

/** The free tier's three limits (5-hour, weekly, monthly), in weighted tokens. */
export function Meters({ s }: { s: InferenceStatus }) {
  const caps = s.free.usage?.caps;
  if (!caps) return null;
  return (
    <div className="meters">
      <Bar label="5-hour" m={caps.five_hour} />
      <Bar label="Weekly" m={caps.weekly} />
      <Bar label="Monthly" m={caps.monthly} />
      <p className="muted small">
        Tokens are weighted by model size. A task that is mid-way when a limit is reached may finish; what it uses
        counts toward your next window.
      </p>
    </div>
  );
}

/** Asked when no model can run the task: the ways forward, never an error. */
export function PromptCard({
  prompt,
  onSetup,
  onClose,
}: {
  prompt: TaskPrompt;
  onSetup: (path: InfPath) => void;
  onClose: () => void;
}) {
  return (
    <div className="notice-card notice-info prompt-card" role="status" data-testid="task-prompt">
      <div className="notice-head">
        <span>{prompt.message}</span>
        <button className="icon-btn" aria-label="Dismiss" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="notice-body">
        <p className="muted small">Pick a way to run it. Your task text is kept.</p>
        <div className="prompt-options">
          {prompt.options.map((o) => (
            <button
              key={o.path}
              className="prompt-option"
              onClick={() => onSetup(o.path)}
              disabled={o.path === "A" && !o.ready}
            >
              <b>{o.title}</b>
              <span className="muted small">{o.detail}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/**
 * First run: nothing set up yet. No account, no key asked for: local comes
 * first (free, private), then a key, then the free tier if it is offered.
 */
export function GetStarted({ s, onSetup }: { s: InferenceStatus; onSetup: (path: InfPath) => void }) {
  const anyReady = s.local.ready.length > 0 || s.byok.some((b) => b.ready) || s.free.state.state === "available";
  if (s.source !== "routed" || anyReady) return null;
  return (
    <div className="notice-card get-started" data-testid="get-started">
      <div className="notice-head">
        <span>Choose how Anchor runs — no account needed</span>
      </div>
      <div className="notice-body">
        <div className="prompt-options">
          <button className="prompt-option" onClick={() => onSetup("C")}>
            <b>Run locally — nothing leaves your machine</b>
            <span className="muted small">
              {s.local.running
                ? "Ollama is running. Pull two models of different families, e.g. qwen2.5-coder:7b and llama3.1:8b."
                : "Install Ollama from ollama.com, then pull two models (e.g. qwen2.5-coder:7b and llama3.1:8b)."}
            </span>
          </button>
          <button className="prompt-option" onClick={() => onSetup("B")}>
            <b>Use your own API key</b>
            <span className="muted small">Billed by your provider. The key stays in your system keychain.</span>
          </button>
          {s.free.offered && (
            <button className="prompt-option" onClick={() => onSetup("A")}>
              <b>Start free — no signup</b>
              <span className="muted small">A capped free tier, for trying Anchor out.</span>
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/** Shown once, the first time a task fails: what Anchor gives you then. */
export function FirstFailure({ onClose }: { onClose: () => void }) {
  return (
    <div className="notice-card notice-info" role="status" data-testid="first-failure">
      <div className="notice-head">
        <span>That didn't work — and nothing is lost</span>
        <button className="icon-btn" aria-label="Dismiss" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="notice-body">
        <ul className="plain-list">
          <li>
            <b>Undo:</b> every turn that changed files has a snapshot. Use “Undo from here” on any turn to put your
            files back exactly as they were.
          </li>
          <li>
            <b>The Reviewer:</b> a second model, from a different family, checks the work before it counts, so a
            model never grades itself.
          </li>
          <li>
            <b>Evidence:</b> tests and checks are run for real and recorded; the Audit log (top right) shows every
            step, in a tamper-evident chain.
          </li>
        </ul>
      </div>
    </div>
  );
}

const SEEN = "anchor.first-failure-seen";

export function firstFailureSeen(): boolean {
  try {
    return localStorage.getItem(SEEN) === "1";
  } catch {
    return false;
  }
}

export function markFirstFailureSeen() {
  try {
    localStorage.setItem(SEEN, "1");
  } catch {
    /* storage unavailable: it may show again next time */
  }
}
