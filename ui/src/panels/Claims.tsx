import { type ReactElement, useEffect, useMemo, useState } from "react";
import { get } from "../api";
import type { Claim, ClaimsView, Conflict } from "../types";
import { AgentChip, Badge, Empty, ErrorLine, show, time } from "../util";

interface Node {
  name: string;
  path: string;
  dir: boolean;
  children: Map<string, Node>;
}

function covers(claim: string, path: string, dir: boolean): boolean {
  if (claim === "." || claim === "" || claim === "/") return true;
  if (claim.endsWith("/")) return (dir ? `${path}/` : path).startsWith(claim) || `${path}/` === claim;
  return claim === path;
}

function buildTree(paths: string[]): Node {
  const root: Node = { name: "", path: "", dir: true, children: new Map() };
  for (const p of paths) {
    const parts = p.replace(/\/$/, "").split("/").filter(Boolean);
    let n = root;
    parts.forEach((part, i) => {
      const isDir = i < parts.length - 1 || p.endsWith("/");
      let c = n.children.get(part);
      if (!c) {
        c = { name: part, path: parts.slice(0, i + 1).join("/"), dir: isDir, children: new Map() };
        n.children.set(part, c);
      }
      if (isDir) c.dir = true;
      n = c;
    });
  }
  return root;
}

/** The file claim map (§12.2): the workspace tree colored by the agent
 * holding each file's claim; refused claims and merge conflicts in red. */
export function Claims({
  version,
  colors,
  onInspectFile,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  onInspectFile?: (path: string) => void;
}) {
  const [view, setView] = useState<ClaimsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  // After a run every claim is settled: show them all by default then.
  const [historyChoice, setHistory] = useState<boolean | null>(null);
  useEffect(() => {
    get<ClaimsView>("/api/claims").then(
      (v) => {
        setView(v);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version]);
  const active = useMemo(
    () => (view?.claims ?? []).filter((c) => c.status === "active" && !c.lease_expired),
    [view],
  );
  const history = historyChoice ?? active.length === 0;
  const shown = history ? (view?.claims ?? []) : active;
  const conflictPaths = useMemo(() => {
    const m = new Map<string, Conflict[]>();
    for (const c of view?.conflicts ?? []) {
      const ps = c.path ? [c.path] : (c.paths ?? []);
      for (const p of ps) m.set(p.replace(/\/$/, ""), [...(m.get(p.replace(/\/$/, "")) ?? []), c]);
    }
    return m;
  }, [view]);
  const tree = useMemo(
    () => buildTree([...(view?.files ?? []), ...shown.map((c) => c.path), ...conflictPaths.keys()]),
    [view, shown, conflictPaths],
  );
  if (!view) return <ErrorLine error={error} />;

  const owners = (path: string, dir: boolean): Claim[] =>
    shown.filter((c) => covers(c.path, path, dir) && (c.claim_type === "edit" || history));

  const render = (n: Node, depth: number): ReactElement[] => {
    const rows: ReactElement[] = [];
    const kids = [...n.children.values()].sort((a, b) => Number(b.dir) - Number(a.dir) || a.name.localeCompare(b.name));
    for (const k of kids) {
      const own = owners(k.path, k.dir);
      const conflicts = conflictPaths.get(k.path) ?? [];
      const editors = new Set(own.filter((c) => c.claim_type === "edit" && c.status === "active").map((c) => c.agent));
      const clash = editors.size > 1 || conflicts.length > 0;
      const color = own[0] ? colors(own[0].agent) : undefined;
      // A claim is listed where it was made; the files inside a claimed
      // directory carry its color.
      const here = own.filter((c) => c.path.replace(/\/$/, "") === k.path);
      rows.push(
        <li
          key={k.path}
          className={`tree-row ${clash ? "tree-conflict" : ""}`}
          style={{ paddingLeft: `${depth * 1.1 + 0.4}rem`, borderLeftColor: color ?? "transparent" }}
          data-testid={`tree-${k.path}`}
          data-conflict={clash ? "true" : "false"}
        >
          <span className="tree-name">
            {k.dir ? "▸ " : ""}
            {k.name}
            {k.dir ? "/" : ""}
          </span>
          {!k.dir && onInspectFile && (
            <button type="button" className="link small" onClick={() => onInspectFile(k.path)}>
              open in workbench
            </button>
          )}
          {here.map((c) => (
            <span key={c.id} className={c.status === "active" ? "" : "faded"}>
              <AgentChip name={c.agent} color={colors(c.agent)} />
              <span className="small muted">
                {" "}
                {c.claim_type}
                {c.status !== "active" ? ` (${c.status})` : ""}
              </span>
            </span>
          ))}
          {conflicts.map((c) => (
            <Badge key={c.seq} kind="fail">
              {c.kind === "claim_refused" ? `${c.agent ?? "?"}'s claim refused` : `conflict in merge #${c.merge}`}
            </Badge>
          ))}
        </li>,
      );
      if (k.dir) rows.push(...render(k, depth + 1));
    }
    return rows;
  };

  return (
    <div>
      <ErrorLine error={error} />
      <div className="filters">
        <label>
          <input type="checkbox" checked={history} onChange={(e) => setHistory(e.target.checked)} /> include released
          and expired claims
        </label>
        <span className="muted small">
          {active.length} active claim{active.length === 1 ? "" : "s"} · {view.conflicts.length} conflict
          {view.conflicts.length === 1 ? "" : "s"}
        </span>
      </div>
      <div className="two">
        <ul className="tree" aria-label="Workspace files">
          {render(tree, 0)}
        </ul>
        <div>
          <h3>Conflicts</h3>
          {!view.conflicts.length && <Empty>No conflicts.</Empty>}
          <ul className="list">
            {view.conflicts.map((c) => (
              <li key={c.seq} className="conflict" data-testid="conflict">
                <AgentChip name={c.agent} color={colors(c.agent)} />{" "}
                {c.kind === "claim_refused" ? (
                  <>
                    asked for <code>{c.path}</code>: <span className="fail">{show(c.reason)}</span>
                  </>
                ) : (
                  <>
                    merge #{c.merge} rejected ({show(c.reason)}): <code>{(c.paths ?? []).join(", ")}</code>
                  </>
                )}
              </li>
            ))}
          </ul>
          <h3>All claims</h3>
          <table className="table">
            <thead>
              <tr>
                <th>#</th>
                <th>agent</th>
                <th>path</th>
                <th>type</th>
                <th>state</th>
                <th>since</th>
              </tr>
            </thead>
            <tbody>
              {view.claims.map((c) => (
                <tr key={c.id}>
                  <td>{c.id}</td>
                  <td>
                    <AgentChip name={c.agent} color={colors(c.agent)} />
                  </td>
                  <td>
                    <code>{c.path}</code>
                  </td>
                  <td>{c.claim_type}</td>
                  <td>{c.status === "active" && c.lease_expired ? "lease expired" : c.status}</td>
                  <td>{time(c.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
