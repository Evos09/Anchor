import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, get, post, q } from "../api";
import type { AppState, DirListing, Event, Overview, Pending, SnapshotsView } from "../types";
import { Badge, Empty, ErrorLine, makeColors, time, usd } from "../util";
import { Snapshots } from "./Snapshots";
import { Audit } from "./Audit";
import { Cost } from "./Cost";

const LABEL: Record<string, string> = {
  approve: "Approve",
  deny: "Deny",
  continue: "Continue (compact)",
  fresh: "Stop; fresh session later",
  rollback: "Stop; roll back",
};

/** What the conversation shows (the rest is in the Audit tab). */
function shown(e: Event): boolean {
  if (e.level === "trace") return false;
  return !["usage", "approval_pending", "approval_resolved", "snapshot", "checkpoint_saved"].includes(e.kind);
}

/** The desktop app (ADR-0028): open a project, give the agent a task,
 * answer its approvals, undo its turns. */
export function Workspace() {
  const [app, setApp] = useState<AppState | null>(null);
  const [overview, setOverview] = useState<Overview | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [picker, setPicker] = useState(false);
  const [side, setSide] = useState<"" | "snapshots" | "audit" | "cost">("");
  const lastSeq = useRef(-1);

  const refresh = useCallback(async () => {
    try {
      const a = await get<AppState>("/api/app/state");
      setApp(a);
      if (a.project) setOverview(await get<Overview>("/api/overview"));
      else setOverview(null);
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError && e.status === 401 ? "unauthorized" : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // The live feed for the open project; starts over when it changes.
  const project = app?.project ?? null;
  useEffect(() => {
    if (!project) return;
    let stop = false;
    lastSeq.current = -1;
    setEvents([]);
    (async () => {
      let wait: number | undefined = undefined;
      while (!stop) {
        try {
          const page: { events: Event[]; more: boolean } = await get<{ events: Event[]; more: boolean }>(
            `/api/events${q({ after: lastSeq.current, wait })}`,
          );
          if (stop) break;
          wait = page.more ? undefined : 1;
          if (page.events.length) {
            lastSeq.current = page.events[page.events.length - 1]!.seq;
            setEvents((old) => old.concat(page.events));
          }
          await refresh();
        } catch (e) {
          if (stop) break;
          setError(e instanceof ApiError && e.status === 401 ? "unauthorized" : "connection lost; retrying");
          await new Promise((r) => setTimeout(r, 2000));
        }
      }
    })();
    return () => {
      stop = true;
    };
  }, [project, refresh]);

  if (error === "unauthorized") {
    return (
      <main className="gate">
        <h1>Anchor</h1>
        <p>This page needs its access token: open the full URL that <code>anchor app</code> printed.</p>
      </main>
    );
  }
  const running = app?.running ?? false;
  const version = overview?.last_seq ?? -1;
  return (
    <div className="app">
      <header className="top">
        <div className="title">
          <h1>Anchor</h1>
          {project ? (
            <span className="ws" title={project} data-testid="project">
              {project}
            </span>
          ) : (
            <span className="muted">no project open</span>
          )}
          <button className="btn btn-small" onClick={() => setPicker(true)} disabled={running}>
            Open folder…
          </button>
          {running && <span className="badge badge-live">working</span>}
        </div>
        {overview && (
          <div className="stats" aria-label="summary">
            <button className="stat" onClick={() => setSide(side === "cost" ? "" : "cost")}>
              <b>{usd(overview.cost_total.spend_usd)}</b> spent
            </button>
            <button className="stat" onClick={() => setSide(side === "snapshots" ? "" : "snapshots")}>
              Snapshots
            </button>
            <button
              className={`stat ${overview.chain.error ? "stat-fail" : "stat-ok"}`}
              onClick={() => setSide(side === "audit" ? "" : "audit")}
              title="The audit log's hash chain, verified as it is read"
            >
              {overview.chain.error ? "audit chain BROKEN" : "audit log"}
            </button>
          </div>
        )}
      </header>
      {error && (
        <div className="banner banner-warn" role="status">
          {error}
        </div>
      )}
      {app && <Models app={app} onChanged={refresh} />}
      {picker && app && (
        <Picker
          app={app}
          onClose={() => setPicker(false)}
          onOpened={async () => {
            setPicker(false);
            await refresh();
          }}
        />
      )}
      <div className={side ? "app-layout app-layout-side" : "app-layout"}>
        <main className="conversation">
          {!project ? (
            <Welcome app={app} onPick={() => setPicker(true)} onOpened={refresh} />
          ) : (
            <Conversation
              events={events}
              pending={overview?.pending ?? []}
              version={version}
              running={running}
              rollbackBlocked={overview?.rollback ?? null}
              onChanged={refresh}
            />
          )}
          {project && <Composer running={running} onChanged={refresh} />}
        </main>
        {side && overview && (
          <aside className="panel side-panel" aria-label={side}>
            <div className="row">
              <h2>{side === "snapshots" ? "Snapshot timeline" : side === "audit" ? "Audit log" : "Cost meter"}</h2>
              <button className="btn btn-small" onClick={() => setSide("")}>
                Close
              </button>
            </div>
            {side === "snapshots" && <Snapshots version={version} colors={makeColors([])} />}
            {side === "audit" && <Audit events={events} overview={overview} />}
            {side === "cost" && <Cost version={version} colors={makeColors([])} />}
          </aside>
        )}
      </div>
    </div>
  );
}

function Welcome({ app, onPick, onOpened }: { app: AppState | null; onPick: () => void; onOpened: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const open = async (path: string) => {
    try {
      await post("/api/app/open", { path });
      onOpened();
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <section className="welcome">
      <h2>Open a project to start</h2>
      <p className="muted">
        Anchor works inside the folder you open. It asks you before it changes a file or runs a command, and every
        turn can be undone.
      </p>
      <button className="btn btn-primary" onClick={onPick}>
        Open folder…
      </button>
      <ErrorLine error={error} />
      {app && app.recent.length > 0 && (
        <>
          <h3>Recent</h3>
          <ul className="recent">
            {app.recent.map((r) => (
              <li key={r}>
                <button className="link" onClick={() => open(r)}>
                  {r}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}

/** The models file: which AI models the agent uses. */
function Models({ app, onChanged }: { app: AppState; onChanged: () => void }) {
  const [edit, setEdit] = useState(false);
  const [path, setPath] = useState(app.models.path);
  const [error, setError] = useState<string | null>(null);
  if (app.models.ok && !edit) {
    return (
      <p className="models small muted">
        Models: <code>{app.models.path}</code>{" "}
        <button className="link" onClick={() => setEdit(true)}>
          change
        </button>
      </p>
    );
  }
  const save = async () => {
    try {
      await post("/api/app/models", { path });
      setError(null);
      setEdit(false);
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <div className={`banner ${app.models.ok ? "" : "banner-warn"} models`} role="status">
      {!app.models.ok && (
        <p>
          The models file can't be used yet: {app.models.error}. Edit it, or point to another one. The default uses
          two free local models through Ollama (see the README).
        </p>
      )}
      <div className="row">
        <input
          className="wide-input"
          aria-label="Models file"
          value={path}
          onChange={(e) => setPath(e.target.value)}
        />
        <button className="btn" onClick={save}>
          Use this file
        </button>
        {edit && (
          <button className="btn" onClick={() => setEdit(false)}>
            Cancel
          </button>
        )}
      </div>
      <ErrorLine error={error} />
    </div>
  );
}

/** Choose a folder: the server lists directory names only. */
function Picker({ app, onClose, onOpened }: { app: AppState; onClose: () => void; onOpened: () => void }) {
  const [listing, setListing] = useState<DirListing | null>(null);
  const [typed, setTyped] = useState("");
  const [error, setError] = useState<string | null>(null);
  // Only the latest browse is shown: a slow earlier reply (the first listing,
  // say) must not replace the folder the user has since gone to.
  const latest = useRef(0);
  const browse = useCallback(async (path?: string) => {
    const n = ++latest.current;
    try {
      const l = await get<DirListing>(`/api/app/dirs${q({ path })}`);
      if (n !== latest.current) return;
      setListing(l);
      if (l.path) setTyped(l.path);
      setError(null);
    } catch (e) {
      if (n === latest.current) setError(String(e));
    }
  }, []);
  // Where the dialog starts, read once when it opens.
  const start = useRef(app.project ?? app.home ?? undefined);
  useEffect(() => {
    void browse(start.current);
  }, [browse]);
  const open = async (path: string) => {
    try {
      await post("/api/app/open", { path });
      onOpened();
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <div className="dialog picker" role="dialog" aria-label="Open a folder">
      <div className="row">
        <h3>Open a folder</h3>
        <button className="btn btn-small" onClick={onClose}>
          Close
        </button>
      </div>
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          void browse(typed);
        }}
      >
        <input
          className="wide-input"
          aria-label="Folder path"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          placeholder="a folder path"
        />
        <button className="btn" type="submit">
          Go
        </button>
      </form>
      <ErrorLine error={error} />
      <div className="row">
        <button className="btn btn-small" onClick={() => browse(undefined)}>
          Start
        </button>
        {listing?.parent && (
          <button className="btn btn-small" onClick={() => browse(listing.parent ?? undefined)}>
            Up
          </button>
        )}
        {listing?.path && (
          <button className="btn btn-primary" onClick={() => open(listing.path!)} data-testid="open-here">
            Open this folder
          </button>
        )}
      </div>
      <ul className="dirs">
        {(listing?.dirs ?? []).map((d, i) => (
          <li key={d}>
            <button className="link" onClick={() => browse(d)}>
              📁 {listing?.names?.[i] ?? d}
            </button>
          </li>
        ))}
        {listing && !listing.dirs.length && <Empty>No folders here.</Empty>}
      </ul>
      {listing?.truncated && <p className="small muted">Only the first 2000 folders are shown.</p>}
      {app.recent.length > 0 && (
        <>
          <h3>Recent</h3>
          <ul className="recent">
            {app.recent.map((r) => (
              <li key={r}>
                <button className="link" onClick={() => open(r)}>
                  {r}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

function Composer({ running, onChanged }: { running: boolean; onChanged: () => void }) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const send = async () => {
    if (!text.trim()) return;
    try {
      await post("/api/app/task", { text });
      setText("");
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    onChanged();
  };
  const stop = async () => {
    try {
      await post("/api/app/cancel", {});
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    onChanged();
  };
  return (
    <div className="composer">
      <ErrorLine error={error} />
      <div className="row">
        <textarea
          aria-label="Task"
          placeholder={running ? "The agent is working…" : "What should the agent do?"}
          value={text}
          disabled={running}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) void send();
          }}
        />
        {running ? (
          <button className="btn btn-danger" onClick={stop}>
            Stop
          </button>
        ) : (
          <button className="btn btn-primary" onClick={send} disabled={!text.trim()}>
            Send
          </button>
        )}
      </div>
      <p className="small muted">Ctrl+Enter sends.</p>
    </div>
  );
}

function Conversation({
  events,
  pending,
  version,
  running,
  rollbackBlocked,
  onChanged,
}: {
  events: Event[];
  pending: Pending[];
  version: number;
  running: boolean;
  rollbackBlocked: string | null;
  onChanged: () => void;
}) {
  const [snaps, setSnaps] = useState<SnapshotsView | null>(null);
  const [prepared, setPrepared] = useState<{ nonce: string; ref: string; changes: string[]; turn: number } | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    get<SnapshotsView>("/api/snapshots").then(setSnaps, (e) => setError(String(e)));
  }, [version]);
  useEffect(() => {
    end.current?.scrollIntoView?.({ block: "end" });
  }, [events.length, pending.length]);
  // The snapshot taken before each turn's first change.
  const turnRef = useMemo(() => {
    const m = new Map<string, string>();
    for (const s of snaps?.snapshots ?? [])
      if (s.kind === "turn" && s.session_id && s.turn !== null && s.restorable) m.set(`${s.session_id}:${s.turn}`, s.ref);
    return m;
  }, [snaps]);

  const answer = async (id: number, choice: string) => {
    setBusy(id);
    try {
      await post("/api/approvals/answer", { id, choice });
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      onChanged();
    }
  };
  const undo = async (ref: string, turn: number) => {
    setNote(null);
    try {
      const r = await post<{ nonce: string; ref: string; changes: string[] }>("/api/rollback/prepare", { ref });
      setPrepared({ ...r, turn });
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };
  const confirm = async () => {
    if (!prepared) return;
    try {
      await post("/api/rollback/confirm", { nonce: prepared.nonce });
      setNote(`Undone: the project is back to how it was before turn ${prepared.turn}.`);
      setPrepared(null);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    onChanged();
  };

  const items = events.filter(shown);
  return (
    <div className="thread" aria-live="polite">
      {!items.length && !running && <Empty>Type a task below to begin.</Empty>}
      {items.map((e) => {
        const d = (e.data ?? {}) as Record<string, unknown>;
        if (e.kind === "ui_action" && d["action"] === "start_task") {
          const args = (d["args"] ?? {}) as { text?: string };
          return (
            <div key={e.seq} className="bubble bubble-user" data-testid="task">
              {args.text}
            </div>
          );
        }
        if (e.kind === "turn_started") {
          const turn = Number(d["turn"]);
          const ref = e.session_id ? turnRef.get(`${e.session_id}:${turn}`) : undefined;
          return (
            <div key={e.seq} className="turn-sep">
              <span>Turn {turn}</span>
              {ref && !running && !rollbackBlocked && (
                <button className="btn btn-small" onClick={() => undo(ref, turn)} data-testid={`undo-${turn}`}>
                  Undo from here
                </button>
              )}
            </div>
          );
        }
        if (e.kind === "session_finished") {
          const result = typeof d["result"] === "string" ? d["result"] : null;
          return (
            <div key={e.seq}>
              {result && (
                <div className="bubble bubble-agent" data-testid="result">
                  {result}
                </div>
              )}
              <p className={`small lvl-${e.level}`}>{e.text}</p>
            </div>
          );
        }
        return (
          <p key={e.seq} className={`ev-line small lvl-${e.level}`}>
            <span className="ev-time">{time(e.ts_ms)}</span> {e.text}
          </p>
        );
      })}
      {pending.map((p) => (
        <article key={p.id} className="approval card" data-testid={`approval-${p.kind}`}>
          <div className="row">
            <Badge kind="warn">needs your OK</Badge>
            <span className="summary">{p.summary}</span>
          </div>
          {p.target && (
            <p className="small">
              <code>{p.target}</code>
            </p>
          )}
          {p.rule && <p className="muted small">rule: {p.rule}</p>}
          {p.detail && (
            <details>
              <summary>Details</summary>
              <pre className="pre">{p.detail}</pre>
            </details>
          )}
          <div className="actions">
            {p.choices.map((c) => (
              <button
                key={c}
                className={c === "approve" || c === "continue" ? "btn btn-primary" : "btn"}
                disabled={busy === p.id}
                onClick={() => answer(p.id, c)}
              >
                {LABEL[c] ?? c}
              </button>
            ))}
          </div>
        </article>
      ))}
      {running && !pending.length && <p className="muted small thinking">The agent is working…</p>}
      <ErrorLine error={error} />
      {note && (
        <p className="ok" role="status">
          {note}
        </p>
      )}
      {prepared && (
        <div className="dialog" role="dialog" aria-label="Confirm undo">
          <h3>Undo turn {prepared.turn} and everything after it?</h3>
          <p>These {prepared.changes.length} path(s) go back to how they were (atomic, verified, audited):</p>
          <pre className="pre">{prepared.changes.join("\n") || "(nothing to change)"}</pre>
          <div className="actions">
            <button className="btn btn-danger" onClick={confirm}>
              Undo
            </button>
            <button className="btn" onClick={() => setPrepared(null)}>
              Keep the changes
            </button>
          </div>
        </div>
      )}
      <div ref={end} />
    </div>
  );
}
