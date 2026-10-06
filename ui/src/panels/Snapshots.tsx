import { useEffect, useState } from "react";
import { get, post, q } from "../api";
import type { Snapshot, SnapshotsView } from "../types";
import { AgentChip, Badge, Empty, ErrorLine, dateTime } from "../util";
import { Diff } from "./MergeQueue";

const KIND: Record<string, string> = {
  turn: "before a turn",
  merge: "after a merge",
  "merge-pre": "before a merge",
  "merge-candidate": "merge candidate",
  head: "latest",
  iso: "isolated workspace",
  other: "snapshot",
};

/** The snapshot timeline (§12.2): every snapshot, the agent turn or merge
 * that produced it, what changed, and a restore (two steps: preview, then
 * confirm). */
export function Snapshots({
  version,
  colors,
  focus,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  focus?: string;
}) {
  const [v, setV] = useState<SnapshotsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [prepared, setPrepared] = useState<{ nonce: string; ref: string; changes: string[] } | null>(null);
  const [diff, setDiff] = useState<{ key: string; text: string } | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    get<SnapshotsView>("/api/snapshots").then(
      (x) => {
        setV(x);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version, tick]);
  useEffect(() => {
    if (focus) void prepare(focus);
  }, [focus]);
  async function prepare(ref: string) {
    setDone(null);
    try {
      setPrepared(await post<{ nonce: string; ref: string; changes: string[] }>("/api/rollback/prepare", { ref }));
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }
  const confirm = async () => {
    if (!prepared) return;
    try {
      const r = await post<{ operations: string[] }>("/api/rollback/confirm", { nonce: prepared.nonce });
      setDone(`Restored ${prepared.ref} (${r.operations.length} operations).`);
      setPrepared(null);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    setTick((t) => t + 1);
  };
  const showDiff = async (s: Snapshot, path: string) => {
    if (!s.previous) return;
    try {
      const r = await get<{ diff: string }>(`/api/snapshot-diff${q({ a: s.previous, b: s.ref, path })}`);
      setDiff({ key: `${s.ref}:${path}`, text: r.diff });
    } catch (e) {
      setError(String(e));
    }
  };
  if (!v) return <ErrorLine error={error} />;
  if (!v.available) return <p className="banner banner-warn">Snapshots are not available here: {v.note}</p>;
  const rows = [...v.snapshots].reverse();
  return (
    <div>
      <ErrorLine error={error} />
      {done && (
        <p className="ok" role="status">
          {done}
        </p>
      )}
      {v.rollback && <p className="muted small">Restore is unavailable now: {v.rollback}.</p>}
      {prepared && (
        <div className="dialog" role="dialog" aria-label="Confirm restore">
          <h3>Restore the workspace to {prepared.ref}?</h3>
          <p>This changes {prepared.changes.length} path(s) in the main workspace (atomic, verified, audited):</p>
          <pre className="pre">{prepared.changes.join("\n") || "(nothing to change)"}</pre>
          <div className="actions">
            <button className="btn btn-danger" onClick={confirm}>
              Restore
            </button>
            <button className="btn" onClick={() => setPrepared(null)}>
              Cancel
            </button>
          </div>
        </div>
      )}
      {!rows.length && <Empty>No snapshots yet.</Empty>}
      <ol className="timeline">
        {rows.map((s) => (
          <li key={s.ref} className="snap" data-testid={`snapshot-${s.ref}`}>
            <div className="row">
              <span className="muted small">{dateTime(s.created_ms)}</span>
              <Badge kind={s.kind.startsWith("merge") ? "info" : "muted"}>{KIND[s.kind] ?? s.kind}</Badge>
              {s.agent && <AgentChip name={s.agent} color={colors(s.agent)} />}
              <code className="small">{s.ref}</code>
              {s.restorable && !v.rollback && (
                <button className="btn btn-small" onClick={() => prepare(s.ref)} data-testid="restore">
                  Restore
                </button>
              )}
            </div>
            <div className="small">
              {s.label}
              {s.turn !== null ? ` (turn ${s.turn})` : ""}
              {s.merge_seq !== null ? ` · merge #${s.merge_seq}` : ""} · {s.files} files
            </div>
            {s.changes.length > 0 && (
              <ul className="changes">
                {s.changes.map((c) => (
                  <li key={c.path}>
                    <span className={`chg chg-${c.kind}`}>{c.kind}</span>{" "}
                    <button className="link" onClick={() => showDiff(s, c.path)}>
                      {c.path}
                    </button>
                    {diff?.key === `${s.ref}:${c.path}` && <Diff text={diff.text} />}
                  </li>
                ))}
              </ul>
            )}
            {s.previous && !s.changes.length && <div className="small muted">no changes since {s.previous}</div>}
          </li>
        ))}
      </ol>
    </div>
  );
}
