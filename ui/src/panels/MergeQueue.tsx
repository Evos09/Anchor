import { useEffect, useState } from "react";
import { get, q } from "../api";
import type { Merge, MergeDetail } from "../types";
import { AgentChip, Badge, Empty, ErrorLine, duration, show, time } from "../util";

/** The merge queue (§12.2): each change waiting to integrate, its agent,
 * verification result and expected merge time; rejected ones say why. */
export function MergeQueue({
  version,
  colors,
  focus,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  focus?: number;
}) {
  const [merges, setMerges] = useState<Merge[]>([]);
  const [median, setMedian] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<number | null>(focus ?? null);
  useEffect(() => setOpen(focus ?? null), [focus]);
  useEffect(() => {
    get<{ merges: Merge[]; median_gate_ms: number | null }>("/api/merges").then(
      (v) => {
        setMerges(v.merges);
        setMedian(v.median_gate_ms);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version]);
  return (
    <div>
      <ErrorLine error={error} />
      <p className="muted small">
        Changes are merged one at a time, each verified first. Typical verification time so far: {duration(median)}.
      </p>
      {!merges.length && <Empty>Nothing submitted yet.</Empty>}
      <table className="table">
        <thead>
          <tr>
            <th>#</th>
            <th>agent</th>
            <th>status</th>
            <th>verification</th>
            <th>expected</th>
            <th>submitted</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {merges.map((m) => (
            <tr key={m.seq} className={m.status === "rejected" ? "row-fail" : ""} data-testid={`merge-${m.seq}`}>
              <td>{m.seq}</td>
              <td>
                <AgentChip name={m.agent} color={colors(m.agent)} />
                {m.kind === "revert" && <Badge kind="warn">revert of #{m.reverts}</Badge>}
              </td>
              <td>
                <Badge kind={m.status === "merged" ? "ok" : m.status === "rejected" ? "fail" : "info"}>{m.status}</Badge>
              </td>
              <td>
                {verification(m)}
                {m.caught_later && (
                  <Badge kind="fail">caught later: tests fail on code already merged</Badge>
                )}
              </td>
              <td>{m.expected_ms !== null ? `≈ ${duration(m.expected_ms)}` : "—"}</td>
              <td>{time(m.created_at)}</td>
              <td>
                <button className="link" onClick={() => setOpen(open === m.seq ? null : m.seq)}>
                  {open === m.seq ? "hide" : "diff & evidence"}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {open !== null && <MergeEvidence seq={open} />}
    </div>
  );
}

function verification(m: Merge) {
  const fb = m.feedback as {
    reason?: string;
    verification?: { approved?: boolean; reviewer?: string; tests?: number } | null;
    detail?: unknown;
  };
  if (m.status === "merged") {
    const v = fb.verification;
    if (!v) return <Badge kind="warn">not verified</Badge>;
    return (
      <span>
        <Badge kind="ok">passed</Badge> <span className="small muted">{v.reviewer} reviewer · {v.tests} tests</span>
      </span>
    );
  }
  if (m.status === "rejected") {
    return (
      <span>
        <Badge kind="fail">rejected: {fb.reason}</Badge>{" "}
        <span className="small">{m.findings.join(", ") || show(fb.detail).slice(0, 120)}</span>
      </span>
    );
  }
  return <span className="muted small">pending</span>;
}

/** The change and the empirical evidence, side by side (§12.2). */
function MergeEvidence({ seq }: { seq: number }) {
  const [d, setD] = useState<MergeDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [log, setLog] = useState<{ path: string; text: string } | null>(null);
  useEffect(() => {
    setD(null);
    setLog(null);
    get<MergeDetail>(`/api/merge${q({ seq })}`).then(setD, (e) => setError(String(e)));
  }, [seq]);
  if (error) return <ErrorLine error={error} />;
  if (!d) return <p className="muted">Loading merge #{seq}…</p>;
  const fb = d.merge.feedback as { detail?: { failing_tests?: string[] } };
  const openLog = async (path: string) => {
    try {
      const r = await get<{ text: string }>(`/api/evidence${q({ path })}`);
      setLog({ path, text: r.text });
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <section className="evidence" aria-label={`Merge ${seq}: diff and evidence`} data-testid="merge-evidence">
      <h3>
        Merge #{seq} · {d.merge.agent ?? "?"} · {d.merge.status}
      </h3>
      {!d.evidence_group && (
        <p className="muted">
          No verification evidence for this merge{d.merge.status === "rejected" ? "" : " (an unverified merge)"}.
        </p>
      )}
      <div className="side-by-side">
        <div>
          <h4>The change</h4>
          {(d.changes ?? []).length === 0 && <Empty>No file changes recorded.</Empty>}
          {(d.changes ?? []).map((c) => (
            <div key={c.path} className="file">
              <div className="row">
                <code>{c.path}</code> <Badge kind="muted">{c.status}</Badge>
                {c.is_test && <Badge kind="info">test</Badge>}
              </div>
              <Diff text={c.diff} />
            </div>
          ))}
        </div>
        <div>
          <h4>The evidence</h4>
          {d.decision && (
            <p>
              Decision:{" "}
              <Badge kind={d.decision.approved ? "ok" : "fail"}>{d.decision.approved ? "approved" : "rejected"}</Badge>{" "}
              by the {d.decision.reviewer} reviewer
            </p>
          )}
          {(d.decision?.findings ?? []).length > 0 && (
            <ul className="list">
              {d.decision?.findings.map((f, i) => (
                <li key={i} className="fail" data-testid="finding">
                  [{f.stage}] <b>{f.code}</b> <span className="small">{show(f.detail)}</span>
                </li>
              ))}
            </ul>
          )}
          {(fb.detail?.failing_tests ?? []).length > 0 && (
            <p className="fail">Failing tests: {fb.detail?.failing_tests?.join(", ")}</p>
          )}
          <h5>Claims and verdicts</h5>
          {(d.claims ?? []).map((c) => {
            const v = d.decision?.verdicts.find((x) => x.claim_id === c.id);
            return (
              <div key={c.id} className="claim">
                <b>{c.id}</b>: {c.text}{" "}
                {v && <Badge kind={v.verdict === "supported" ? "ok" : "fail"}>{`${v.verdict} (${v.reason_code})`}</Badge>}
                <div className="small muted">
                  files {c.files.join(", ")} · tests {c.tests.join(", ") || "none"}
                </div>
              </div>
            );
          })}
          <h5>Tests</h5>
          <ul className="tests">
            {(d.tests ?? []).map((t) => (
              <li key={t.id} className={t.outcome === "passed" ? "ok" : t.outcome === "skipped" ? "muted" : "fail"}>
                {t.outcome === "passed" ? "✔" : t.outcome === "skipped" ? "○" : "✖"} {t.id}{" "}
                <span className="small">{t.outcome}</span>
              </li>
            ))}
          </ul>
          <h5>Commands</h5>
          <ul className="list">
            {(d.commands ?? []).map((c) => (
              <li key={c.name} className={c.ok ? "" : "fail"}>
                {c.ok ? "✔" : "✖"} <b>{c.name}</b> <code className="small">{c.argv.join(" ")}</code> exit {String(c.exit)}
                {c.artifact && (
                  <button className="link small" onClick={() => openLog(c.artifact!)}>
                    output
                  </button>
                )}
              </li>
            ))}
          </ul>
          <h5>Stored evidence (hash-checked)</h5>
          <ul className="list small">
            {d.artifacts.map((a) => (
              <li key={a.id}>
                <button className="link" onClick={() => openLog(a.path)}>
                  {a.path.split("/").pop()}
                </button>{" "}
                <span className="muted">sha256 {a.sha256.slice(0, 12)}…</span>
              </li>
            ))}
          </ul>
          {log && (
            <div>
              <h5>{log.path}</h5>
              <pre className="pre log">{log.text}</pre>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export function Diff({ text }: { text: string }) {
  return (
    <pre className="pre diff">
      {text.split("\n").map((l, i) => (
        <span
          key={i}
          className={
            l.startsWith("+") && !l.startsWith("+++")
              ? "add"
              : l.startsWith("-") && !l.startsWith("---")
                ? "del"
                : l.startsWith("@@")
                  ? "hunk"
                  : ""
          }
        >
          {l}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}
