import { useCallback, useEffect, useState } from "react";
import { get, post } from "../api";
import type { ArtifactRecord, Decision } from "../types";
import { Badge, Empty, ErrorLine } from "../util";
import { MarkdownView } from "../components/MarkdownView";

export function ArtifactsPanel({
  version,
  onExecutePlan,
}: {
  version: number;
  onExecutePlan?: (planPrompt: string) => void;
}) {
  const [artifacts, setArtifacts] = useState<ArtifactRecord[]>([]);
  const [decisions, setDecisions] = useState<Decision[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<"rendered" | "source">("rendered");
  const [composing, setComposing] = useState(false);
  const [name, setName] = useState("implementation_plan");
  const [kind, setKind] = useState<"plan" | "report" | "walkthrough" | "diagram">("plan");
  const [content, setContent] = useState(
    "# Implementation Plan\n\n## Objective\nDescribe the architectural goal and verification criteria.\n\n## File Claims & Blast Radius\n- `calc.py` (bounds verification)\n- `tests/test_clamp.py` (test suite)\n\n## Verification Gate\n- `cargo test --workspace`\n\n> [!NOTE]\n> Read-before-write invariant is enforced across all file mutations.\n",
  );
  const [requestFeedback, setRequestFeedback] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const load = useCallback(async () => {
    try {
      const [artRes, decRes] = await Promise.all([
        get<{ artifacts: ArtifactRecord[] }>("/api/artifacts"),
        get<Decision[]>("/api/decisions"),
      ]);
      setArtifacts(artRes.artifacts);
      setDecisions(decRes);
      if (!selected && artRes.artifacts.length > 0) {
        setSelected(artRes.artifacts[0]!.name);
      }
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, [selected]);

  useEffect(() => {
    void load();
  }, [version, load]);

  const current = artifacts.find((a) => a.name === selected) ?? artifacts[0] ?? null;

  const submitArtifact = async () => {
    try {
      await post("/api/artifacts/create", {
        name: name.trim(),
        kind,
        content,
        request_feedback: requestFeedback,
      });
      setStatusMsg(`Saved artifact '${name.trim()}.md'`);
      setComposing(false);
      setSelected(name.trim());
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const copyContent = async () => {
    if (!current) return;
    try {
      await navigator.clipboard.writeText(current.content);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // fallback
    }
  };

  const downloadArtifact = () => {
    if (!current) return;
    const blob = new Blob([current.content], { type: "text/markdown;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${current.name}.md`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const handleExecute = () => {
    if (!current) return;
    const prompt = `Execute implementation plan from artifact '${current.name}.md':\n\n${current.content}`;
    onExecutePlan?.(prompt);
  };

  return (
    <div>
      <ErrorLine error={error} />
      {statusMsg && (
        <div className="banner banner-ok small" role="status">
          {statusMsg}
        </div>
      )}

      <div className="wb-toolbar">
        <div className="row">
          <strong>ARTIFACTS & IMPLEMENTATION STUDIO</strong>
          <span className="muted small">
            Structured plans, architectural specs, and verification reports
          </span>
        </div>
        <button
          type="button"
          className="btn btn-small btn-primary"
          onClick={() => setComposing((v) => !v)}
        >
          {composing ? "Close composer" : "+ New plan / artifact"}
        </button>
      </div>

      {composing && (
        <div className="card wb-composer">
          <div className="wb-section-head">
            <span>AUTHOR STRUCTURED ARTIFACT</span>
            <span className="mono muted">MARKDOWN</span>
          </div>
          <div className="filters">
            <label>
              Name{" "}
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="implementation_plan"
                aria-label="Artifact name"
              />
            </label>
            <label>
              Kind{" "}
              <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
                <option value="plan">plan</option>
                <option value="report">report</option>
                <option value="walkthrough">walkthrough</option>
                <option value="diagram">diagram</option>
              </select>
            </label>
            <label>
              <input
                type="checkbox"
                checked={requestFeedback}
                onChange={(e) => setRequestFeedback(e.target.checked)}
              />{" "}
              Request user feedback before proceeding
            </label>
          </div>
          <textarea
            className="wb-textarea mono"
            rows={10}
            value={content}
            onChange={(e) => setContent(e.target.value)}
            aria-label="Artifact Markdown Content"
          />
          <div className="actions">
            <button type="button" className="btn btn-primary" onClick={submitArtifact}>
              Save Artifact
            </button>
          </div>
        </div>
      )}

      <div className="wb-split">
        <div className="wb-sidebar">
          <div className="wb-section-head">
            <span>ARTIFACTS</span>
            <span className="mono muted">{artifacts.length}</span>
          </div>
          {!artifacts.length && <Empty>No session artifacts written yet.</Empty>}
          <ul className="wb-filelist">
            {artifacts.map((a) => (
              <li key={a.id}>
                <button
                  type="button"
                  className={current?.name === a.name ? "wb-filebtn wb-filebtn-on" : "wb-filebtn"}
                  onClick={() => setSelected(a.name)}
                >
                  <div className="row" style={{ justifyContent: "space-between" }}>
                    <code>{a.name}.md</code>
                    <Badge kind={a.kind === "plan" ? "info" : "muted"}>{a.kind}</Badge>
                  </div>
                </button>
              </li>
            ))}
          </ul>

          <div className="wb-section-head" style={{ marginTop: "1rem" }}>
            <span>LINKED DECISIONS</span>
            <span className="mono muted">{decisions.length}</span>
          </div>
          <ul className="wb-filelist">
            {decisions.map((d) => (
              <li key={d.id} className="wb-meta-item small">
                <div className="row">
                  <span className="mono">#{d.id}</span>
                  <Badge kind={d.status === "accepted" ? "ok" : d.status === "rejected" ? "fail" : "warn"}>
                    {d.status}
                  </Badge>
                </div>
                <div>{d.summary}</div>
              </li>
            ))}
            {!decisions.length && <li className="small muted">No decisions recorded yet.</li>}
          </ul>
        </div>

        <div className="wb-editor-pane">
          {!current ? (
            <Empty>
              Agents write implementation plans and verification walkthroughs using the{" "}
              <code>write_artifact</code> tool, or you can author one above.
            </Empty>
          ) : (
            <div>
              <div className="wb-toolbar">
                <div className="row">
                  <code className="wb-filepath">{current.name}.md</code>
                  <Badge kind={current.kind === "plan" ? "info" : "ok"}>{current.kind}</Badge>
                  {current.request_feedback && <Badge kind="warn">awaiting approval</Badge>}
                </div>
                <div className="row small">
                  <div className="cmd-modes">
                    <button
                      type="button"
                      className={viewMode === "rendered" ? "mode-pill mode-pill-on" : "mode-pill"}
                      onClick={() => setViewMode("rendered")}
                    >
                      Rendered
                    </button>
                    <button
                      type="button"
                      className={viewMode === "source" ? "mode-pill mode-pill-on" : "mode-pill"}
                      onClick={() => setViewMode("source")}
                    >
                      Raw .md
                    </button>
                  </div>
                  <button type="button" className="btn btn-small" onClick={copyContent}>
                    {copied ? "Copied! ✓" : "Copy"}
                  </button>
                  <button type="button" className="btn btn-small" onClick={downloadArtifact}>
                    Download
                  </button>
                  {current.kind === "plan" && (
                    <button
                      type="button"
                      className="btn btn-small btn-primary"
                      onClick={handleExecute}
                    >
                      Approve & Execute Plan ▸
                    </button>
                  )}
                </div>
              </div>

              <div className="small mono muted" style={{ margin: "0.25rem 0 0.5rem" }}>
                Author: {current.agent ?? "user"} · session {current.session_id} · turn {current.turn}
              </div>

              {viewMode === "rendered" ? (
                <div className="card wb-artifact-rendered">
                  <MarkdownView content={current.content} />
                </div>
              ) : (
                <pre className="pre wb-artifact-body mono">{current.content}</pre>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
