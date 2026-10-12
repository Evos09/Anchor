import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ApiError, get, post, q } from "../api";
import type {
  AppState,
  Collab,
  DirListing,
  Event,
  Overview,
  Pending,
  Preference,
  SnapshotsView,
  TaskPrompt,
  TaskReply,
} from "../types";
import { Badge, Empty, ErrorLine, makeColors, time, usd } from "../util";
import * as I from "../icons";
import { Snapshots } from "./Snapshots";
import { Audit } from "./Audit";
import { Cost } from "./Cost";
import { Settings, applyTheme, type Section } from "./Settings";
import {
  FirstFailure,
  GetStarted,
  PATH_NAME,
  PromptCard,
  firstFailureSeen,
  markFirstFailureSeen,
  useInference,
} from "./Inference";

const LABEL: Record<string, string> = {
  approve: "Approve",
  deny: "Deny",
  continue: "Continue (compact)",
  fresh: "Stop; fresh session later",
  rollback: "Stop; roll back",
};

/** What the conversation shows (the rest is in the Audit log). */
function shown(e: Event): boolean {
  if (e.level === "trace") return false;
  // The reviewer's own session shows as its verdicts (pair_review).
  if (e.agent === "reviewer") return false;
  if (e.kind === "ui_action" && (e.data as { action?: string } | null)?.action === "open_project") return false;
  return !["usage", "approval_pending", "approval_resolved", "snapshot", "checkpoint_saved"].includes(e.kind);
}

type Status = "running" | "done" | "failed" | "stopped";

/** One task the user gave the agent, and everything logged until the next. */
interface Task {
  n: number;
  start: Event;
  text: string;
  events: Event[];
  status: Status;
}

/** The open project's tasks, oldest first, from its event feed (the audit
 * log): each `start_task` action begins one, its `session_finished` says how
 * it ended. */
function tasksOf(events: Event[], running: boolean): Task[] {
  const out: Task[] = [];
  for (const e of events) {
    const d = (e.data ?? {}) as { action?: string; args?: { text?: string } };
    if (e.kind === "ui_action" && d.action === "start_task") {
      out.push({ n: out.length, start: e, text: d.args?.text ?? "", events: [], status: "failed" });
    } else {
      out[out.length - 1]?.events.push(e);
    }
  }
  out.forEach((t, i) => {
    // The builder's last session says how it ended (builder + checker runs
    // one per round; the reviewer's sessions are its own).
    const fins = t.events.filter((e) => e.kind === "session_finished" && e.agent !== "reviewer");
    const team = t.events.some((e) => e.kind === "team_started");
    const teamDone = t.events.some((e) => e.kind === "team_finished");
    const fin = team && !teamDone && running && i === out.length - 1 ? undefined : fins[fins.length - 1];
    const d = (fin?.data ?? {}) as { failed?: string | null; error?: string | null; done?: boolean };
    if (!fin) t.status = running && i === out.length - 1 ? "running" : "failed";
    else if (d.failed && /cancelled/.test(d.failed)) t.status = "stopped";
    else if (d.failed || d.error) t.status = "failed";
    else t.status = "done";
  });
  return out;
}

export function basename(p: string): string {
  return p.split(/[\\/]+/).filter(Boolean).pop() ?? p;
}

function ago(ms: number): string {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  if (s < 86400 * 30) return `${Math.floor(s / 86400)}d`;
  return `${Math.floor(s / (86400 * 30))}mo`;
}

const STATUS_TEXT: Record<Status, string> = {
  running: "working",
  done: "done",
  failed: "failed",
  stopped: "stopped",
};

type View =
  | { kind: "home" }
  | { kind: "task"; n: number | null } // null: the latest task
  | { kind: "history" }
  | { kind: "settings"; section: Section };

type Tool = "" | "snapshots" | "audit" | "cost";

/** The desktop app (ADR-0028): projects and their tasks on the left; give
 * the agent a task, answer its approvals, undo its turns. */
export function Workspace() {
  const [app, setApp] = useState<AppState | null>(null);
  const [overview, setOverview] = useState<Overview | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [picker, setPicker] = useState(false);
  const [tool, setTool] = useState<Tool>("");
  const [view, setView] = useState<View>({ kind: "home" });
  const [collapsed, setCollapsed] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [failureSeen, setFailureSeen] = useState(firstFailureSeen);
  const lastSeq = useRef(-1);

  useEffect(() => applyTheme(), []);

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
    setView((v) => (v.kind === "settings" ? v : { kind: "home" }));
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

  const running = app?.running ?? false;
  const tasks = useMemo(() => tasksOf(events, running), [events, running]);

  if (error === "unauthorized") {
    return (
      <main className="gate">
        <h1>Anchor</h1>
        <p>This page needs its access token: open the full URL that <code>anchor app</code> printed.</p>
      </main>
    );
  }

  const openProject = async (path: string) => {
    if (app?.missing.includes(path)) {
      setNotice(`${path} does not exist any more (moved or deleted?).`);
      return;
    }
    try {
      await post("/api/app/open", { path });
      setNotice(null);
      setView({ kind: "home" });
    } catch (e) {
      setNotice(String(e));
    }
    await refresh();
  };

  if (view.kind === "settings" && app) {
    return (
      <div className="desk">
        <Settings
          app={app}
          overview={overview}
          section={view.section}
          onSection={(section) => setView({ kind: "settings", section })}
          onClose={() => setView({ kind: "home" })}
          onChanged={refresh}
          onOpen={openProject}
        />
      </div>
    );
  }

  const version = overview?.last_seq ?? -1;
  const pending = overview?.pending ?? [];
  const latest = tasks.length - 1;
  const current = view.kind === "task" ? (view.n ?? latest) : null;
  const shownTask = current !== null && current >= 0 ? tasks[current] : undefined;
  const onLatest = shownTask !== undefined && shownTask.n === latest;
  const waitingElsewhere = pending.length > 0 && !onLatest;

  return (
    <div className={collapsed ? "desk desk-collapsed" : "desk"}>
      {!collapsed && (
        <Sidebar
          app={app}
          tasks={tasks}
          current={shownTask?.n ?? null}
          view={view}
          running={running}
          onCollapse={() => setCollapsed(true)}
          onNew={() => setView({ kind: "home" })}
          onHistory={() => setView({ kind: "history" })}
          onTask={(n) => setView({ kind: "task", n })}
          onOpen={openProject}
          onPick={() => setPicker(true)}
          onSettings={() => setView({ kind: "settings", section: "general" })}
        />
      )}
      <div className="desk-main">
        <header className="desk-top">
          {collapsed && (
            <button className="icon-btn" aria-label="Show the sidebar" onClick={() => setCollapsed(false)}>
              <I.PanelLeft />
            </button>
          )}
          <div className="desk-crumb">
            {project ? (
              <span data-testid="project" title={project}>
                <b>{basename(project)}</b> <span className="muted small">{project}</span>
              </span>
            ) : (
              <b>Anchor</b>
            )}
            {running && <span className="badge badge-live">working</span>}
          </div>
          {waitingElsewhere && (
            <button className="btn btn-small btn-attn" onClick={() => setView({ kind: "task", n: null })}>
              The agent is waiting for your OK
            </button>
          )}
          {overview && (
            <div className="desk-tools" role="toolbar" aria-label="Tools">
              <button
                className={tool === "snapshots" ? "tool tool-on" : "tool"}
                aria-pressed={tool === "snapshots"}
                onClick={() => setTool(tool === "snapshots" ? "" : "snapshots")}
              >
                <I.Camera /> Snapshots
              </button>
              <button
                className={`tool ${tool === "audit" ? "tool-on" : ""} ${overview.chain.error ? "tool-fail" : ""}`}
                aria-pressed={tool === "audit"}
                onClick={() => setTool(tool === "audit" ? "" : "audit")}
                title="The audit log's hash chain, verified as it is read"
              >
                <I.Shield /> {overview.chain.error ? "Audit chain BROKEN" : "Audit log"}
              </button>
              <button
                className={tool === "cost" ? "tool tool-on" : "tool"}
                aria-pressed={tool === "cost"}
                onClick={() => setTool(tool === "cost" ? "" : "cost")}
              >
                <I.Coins /> {usd(overview.cost_total.spend_usd, 2)}
              </button>
            </div>
          )}
        </header>
        {error && (
          <div className="notice notice-warn" role="status">
            {error}
          </div>
        )}
        <div className={tool && overview ? "desk-body desk-body-drawer" : "desk-body"}>
          <main className="desk-content">
            {notice && (
              <Notice title="Missing folder" onClose={() => setNotice(null)}>
                <I.Folder /> {notice}
              </Notice>
            )}
            {!project ? (
              <Welcome app={app} onPick={() => setPicker(true)} onOpen={openProject} />
            ) : view.kind === "history" ? (
              <HistoryList tasks={tasks} onTask={(n) => setView({ kind: "task", n })} />
            ) : shownTask ? (
              <>
                {shownTask.status === "failed" && !failureSeen && (
                  <FirstFailure
                    onClose={() => {
                      markFirstFailureSeen();
                      setFailureSeen(true);
                    }}
                  />
                )}
                <Conversation
                  key={shownTask.n}
                  events={[shownTask.start, ...shownTask.events]}
                  pending={onLatest ? pending : []}
                  version={version}
                  running={running && onLatest}
                  rollbackBlocked={overview?.rollback ?? null}
                  onChanged={refresh}
                />
                <Composer
                  app={app}
                  running={running}
                  onSent={() => setView({ kind: "task", n: null })}
                  onChanged={refresh}
                  onModels={() => setView({ kind: "settings", section: "models" })}
                />
              </>
            ) : (
              <Home
                app={app}
                project={project}
                running={running}
                onPick={() => setPicker(true)}
                onSent={() => setView({ kind: "task", n: null })}
                onChanged={refresh}
                onModels={() => setView({ kind: "settings", section: "models" })}
              />
            )}
          </main>
          {tool && overview && (
            <aside className="drawer" aria-label={tool}>
              <div className="row drawer-head">
                <h2>{tool === "snapshots" ? "Snapshot timeline" : tool === "audit" ? "Audit log" : "Cost meter"}</h2>
                <button className="icon-btn" aria-label="Close" onClick={() => setTool("")}>
                  <I.Close />
                </button>
              </div>
              {tool === "snapshots" && <Snapshots version={version} colors={makeColors([])} />}
              {tool === "audit" && <Audit events={events} overview={overview} />}
              {tool === "cost" && <Cost version={version} colors={makeColors([])} />}
            </aside>
          )}
        </div>
      </div>
      {picker && app && (
        <Picker
          app={app}
          onClose={() => setPicker(false)}
          onOpened={async () => {
            setPicker(false);
            setNotice(null);
            setView({ kind: "home" });
            await refresh();
          }}
        />
      )}
    </div>
  );
}

function Sidebar({
  app,
  tasks,
  current,
  view,
  running,
  onCollapse,
  onNew,
  onHistory,
  onTask,
  onOpen,
  onPick,
  onSettings,
}: {
  app: AppState | null;
  tasks: Task[];
  current: number | null;
  view: View;
  running: boolean;
  onCollapse: () => void;
  onNew: () => void;
  onHistory: () => void;
  onTask: (n: number) => void;
  onOpen: (path: string) => void;
  onPick: () => void;
  onSettings: () => void;
}) {
  const project = app?.project ?? null;
  const recent = app?.recent ?? [];
  const newest = [...tasks].reverse();
  const SHOWN = 12;
  return (
    <aside className="desk-side" aria-label="Projects">
      <div className="side-brand">
        <I.Logo />
        <span>Anchor</span>
        <button className="icon-btn" aria-label="Hide the sidebar" onClick={onCollapse}>
          <I.PanelLeft />
        </button>
      </div>
      <button className="side-new" onClick={onNew} disabled={!project}>
        <I.Plus /> New task
      </button>
      <button
        className={view.kind === "history" ? "side-item side-on" : "side-item"}
        onClick={onHistory}
        disabled={!project}
      >
        <I.History /> Task history
      </button>
      <div className="side-head">
        <span>Projects</span>
        <button className="icon-btn" aria-label="Open folder…" title="Open folder…" onClick={onPick} disabled={running}>
          <I.FolderPlus />
        </button>
      </div>
      <nav className="side-projects">
        {!recent.length && <p className="side-empty">No projects yet.</p>}
        {recent.map((r) => {
          const open = r === project;
          const gone = app?.missing.includes(r) ?? false;
          return (
            <div key={r} className="side-group">
              <button
                className={`side-project ${open ? "side-project-open" : ""} ${gone ? "side-gone" : ""}`}
                title={gone ? `${r} (folder not found)` : r}
                onClick={() => !open && onOpen(r)}
                disabled={running && !open}
              >
                <I.Folder /> <span className="side-label">{basename(r)}</span>
                {gone && <span className="side-age">not found</span>}
              </button>
              {open &&
                newest.slice(0, SHOWN).map((t) => (
                  <button
                    key={t.n}
                    className={current === t.n && view.kind === "task" ? "side-task side-on" : "side-task"}
                    onClick={() => onTask(t.n)}
                    title={t.text}
                    data-testid="history-item"
                  >
                    <span className={`dot-status dot-${t.status}`} aria-label={STATUS_TEXT[t.status]} />
                    <span className="side-label">{t.text.split("\n")[0]}</span>
                    <span className="side-age">{ago(t.start.ts_ms)}</span>
                  </button>
                ))}
              {open && newest.length > SHOWN && (
                <button className="side-task side-more" onClick={onHistory}>
                  Show all {newest.length}
                </button>
              )}
            </div>
          );
        })}
      </nav>
      <button className="side-item side-settings" onClick={onSettings}>
        <I.Gear /> Settings
      </button>
    </aside>
  );
}

function Notice({
  title,
  kind = "",
  children,
  onClose,
}: {
  title: string;
  kind?: string;
  children: ReactNode;
  onClose?: () => void;
}) {
  return (
    <div className={`notice-card ${kind}`} role="status">
      <div className="notice-head">
        <span>{title}</span>
        {onClose && (
          <button className="icon-btn" aria-label="Dismiss" onClick={onClose}>
            <I.Close />
          </button>
        )}
      </div>
      <div className="notice-body">{children}</div>
    </div>
  );
}

function Welcome({ app, onPick, onOpen }: { app: AppState | null; onPick: () => void; onOpen: (p: string) => void }) {
  return (
    <section className="center welcome">
      <div className="welcome-mark">
        <I.Logo size={40} />
      </div>
      <h2>Open a project to start</h2>
      <p className="muted">
        Anchor works inside the folder you open. It asks you before it changes a file or runs a command, and every
        turn can be undone.
      </p>
      <button className="btn btn-primary" onClick={onPick}>
        Open folder…
      </button>
      {app && app.recent.length > 0 && (
        <div className="card-list">
          <h3>Recent</h3>
          {app.recent.map((r) => (
            <button key={r} className="card-row" onClick={() => onOpen(r)} title={r}>
              <I.Folder /> <b>{basename(r)}</b> <span className="muted small">{r}</span>
              {app.missing.includes(r) && <span className="small fail">not found</span>}
            </button>
          ))}
        </div>
      )}
    </section>
  );
}

/** A new task: the composer in the middle of the window. */
function Home({
  app,
  project,
  running,
  onPick,
  onSent,
  onChanged,
  onModels,
}: {
  app: AppState | null;
  project: string;
  running: boolean;
  onPick: () => void;
  onSent: () => void;
  onChanged: () => void;
  onModels: () => void;
}) {
  return (
    <section className="center home">
      <h2 className="home-title">What should Anchor do in {basename(project)}?</h2>
      {app?.inference?.source === "routed" && <HomeStart onModels={onModels} />}
      {app && app.inference?.source !== "routed" && !app.models.ok && (
        <Notice title="The models aren't ready" kind="notice-warn">
          <p>{app.models.error}</p>
          <p className="muted small">The default uses two free local models through Ollama.</p>
          <div className="actions">
            <button className="btn btn-primary" onClick={onModels}>
              Fix in Settings
            </button>
          </div>
        </Notice>
      )}
      <button className="chip-btn home-project" onClick={onPick} disabled={running} title={project}>
        <I.Folder /> {basename(project)} <I.Chevron open />
      </button>
      <Composer app={app} running={running} big onSent={onSent} onChanged={onChanged} onModels={onModels} />
      <p className="muted small home-hint">
        Anchor asks before every change and command · every turn can be undone · Enter sends, Shift+Enter for a new
        line
      </p>
    </section>
  );
}

function HomeStart({ onModels }: { onModels: () => void }) {
  const [s] = useInference(20000);
  return s ? <GetStarted s={s} onSetup={() => onModels()} /> : null;
}

const PREF_NAME: Record<Preference, string> = {
  auto: "automatic",
  prefer_local: "local first",
  prefer_byok: "your key first",
  free_only: "free tier only",
};

function HistoryList({ tasks, onTask }: { tasks: Task[]; onTask: (n: number) => void }) {
  return (
    <section className="history">
      <h2>Task history</h2>
      {!tasks.length && <Empty>No tasks in this project yet.</Empty>}
      <div className="card-list">
        {[...tasks].reverse().map((t) => (
          <button key={t.n} className="card-row" onClick={() => onTask(t.n)}>
            <span className={`dot-status dot-${t.status}`} aria-hidden="true" />
            <span className="card-row-text">{t.text}</span>
            <span className="muted small">
              {STATUS_TEXT[t.status]} · {new Date(t.start.ts_ms).toLocaleString()}
            </span>
          </button>
        ))}
      </div>
    </section>
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
    <div className="modal">
      <div className="dialog picker" role="dialog" aria-label="Open a folder">
        <div className="row dialog-head">
          <h3>Open a folder</h3>
          <button className="icon-btn" aria-label="Close" onClick={onClose}>
            <I.Close />
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
    </div>
  );
}

const COLLABS: [Collab, string, string][] = [
  ["single", "One AI", "One model does the task."],
  [
    "pair",
    "Builder + checker",
    "One model makes the change; a second model, from another family, reviews it and sends it back until it accepts.",
  ],
  [
    "team",
    "Two builders",
    "Two models work at once in separate copies, splitting the work; you confirm each merge. Or start a task with /team.",
  ],
];

function Composer({
  app,
  running,
  big = false,
  onSent,
  onChanged,
  onModels,
}: {
  app: AppState | null;
  running: boolean;
  big?: boolean;
  onSent: () => void;
  onChanged: () => void;
  onModels: () => void;
}) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [prompt, setPrompt] = useState<TaskPrompt | null>(null);
  const send = async () => {
    if (!text.trim() || running) return;
    try {
      const r = await post<TaskReply>("/api/app/task", { text });
      setError(null);
      if (r.prompt) {
        // Nothing can run it yet: ask, and keep the text.
        setPrompt(r.prompt);
      } else {
        setPrompt(null);
        setText("");
        onSent();
      }
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
  const roles = app?.models.roles;
  const inf = app?.inference;
  const collab: Collab = /^\/team\b/.test(text.trim()) ? "team" : (inf?.collab ?? "pair");
  const setCollab = async (c: Collab) => {
    try {
      await post("/api/app/inference", { collab: c });
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    onChanged();
  };
  const routed = inf?.source === "routed";
  const override = /^\/(local|byok|free)\b/.exec(text.trim())?.[1];
  const chip = routed
    ? override
      ? `This task: ${override === "local" ? "local" : override === "byok" ? "your API key" : "free tier"}`
      : inf?.pin
        ? `Pinned: ${PATH_NAME[inf.pin]}`
        : `Models: ${PREF_NAME[inf?.preference ?? "auto"]}`
    : app?.models.ok
      ? collab === "pair"
        ? `${roles?.executor ?? "executor"} · reviewed by ${roles?.reviewer ?? "reviewer"}`
        : (roles?.executor ?? "executor")
      : "Models not set up";
  return (
    <div className={big ? "composer composer-big" : "composer"}>
      {prompt && <PromptCard prompt={prompt} onSetup={() => onModels()} onClose={() => setPrompt(null)} />}
      <div className="composer-card">
        <textarea
          aria-label="Task"
          placeholder={running ? "The agent is working…" : "Ask Anchor to do something in this project…"}
          value={text}
          disabled={running}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <div className="composer-bar">
          <button
            className={routed || app?.models.ok ? "chip-btn" : "chip-btn chip-bad"}
            onClick={onModels}
            data-testid="path-chip"
            title={
              routed
                ? "Start a task with /local, /byok or /free to choose for that task only"
                : app
                  ? `Models file: ${app.models.path}`
                  : undefined
            }
          >
            <I.Cpu />
            {chip}
            <I.Chevron open />
          </button>
          <div className="seg" role="radiogroup" aria-label="How many AIs" data-testid="collab">
            {COLLABS.map(([c, label, hint]) => (
              <button
                key={c}
                role="radio"
                aria-checked={collab === c}
                className={collab === c ? "seg-btn seg-on" : "seg-btn"}
                title={hint}
                disabled={running}
                onClick={() => setCollab(c)}
              >
                {label}
              </button>
            ))}
          </div>
          <span className="spacer" />
          {running ? (
            <button className="btn btn-danger btn-round-text" onClick={stop}>
              <I.Stop /> Stop
            </button>
          ) : (
            <button className="send-btn" aria-label="Send" title="Send" onClick={send} disabled={!text.trim()}>
              <I.Send />
            </button>
          )}
        </div>
      </div>
      <ErrorLine error={error} />
    </div>
  );
}

/** "Builder A" for a two-builder task's agent (`builder-a-<stamp>`). */
function builderName(agent: string | null): string | null {
  const m = /^builder-([ab])-/.exec(agent ?? "");
  return m ? `Builder ${m[1]!.toUpperCase()}` : null;
}

interface Verdict {
  claim_id: string;
  verdict: "supported" | "refuted" | "unverifiable";
  reason_code: string;
  note?: string;
}

const VERDICT: Record<Verdict["verdict"], string> = {
  supported: "accepted",
  refuted: "not accepted",
  unverifiable: "could not check",
};

/** The checker's answer (builder + checker): a second speaker, claim by claim. */
function ReviewBubble({ e }: { e: Event }) {
  const d = (e.data ?? {}) as {
    round?: number;
    reviewer?: { model?: string; family?: string } | null;
    claims?: { id: string; text: string }[];
    verdicts?: Verdict[];
    approved?: boolean;
    final?: boolean;
  };
  const text = (id: string) => d.claims?.find((c) => c.id === id)?.text ?? id;
  return (
    <div className={`bubble bubble-reviewer ${d.approved ? "bubble-ok" : "bubble-warn"}`} data-testid="review">
      <div className="bubble-who">
        Reviewer · {d.reviewer?.model ?? "another model"}
        {d.reviewer?.family ? ` (${d.reviewer.family})` : ""} · round {d.round}
      </div>
      <p>
        {d.approved
          ? "I checked the change and accept it."
          : d.final
            ? "I still have objections; the rounds are used up. Look over the change before you keep it."
            : "I checked the change and do not accept it yet:"}
      </p>
      <ul className="plain-list">
        {(d.verdicts ?? []).map((v) => (
          <li key={v.claim_id}>
            <b>{VERDICT[v.verdict] ?? v.verdict}:</b> {text(v.claim_id)}
            {v.note ? <span className="muted"> — {v.note}</span> : null}
          </li>
        ))}
      </ul>
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
        if (e.kind === "pair_review") {
          return <ReviewBubble key={e.seq} e={e} />;
        }
        if (e.kind === "pair_reply") {
          return (
            <div key={e.seq} className="turn-sep round-sep" data-testid="round">
              <span>Round {String(d["round"])}: the builder answers the review</span>
            </div>
          );
        }
        if (e.kind === "session_finished") {
          const result = typeof d["result"] === "string" ? d["result"] : null;
          const who = builderName(e.agent);
          return (
            <div key={e.seq}>
              {result && (
                <div className="bubble bubble-agent" data-testid="result">
                  {who && <div className="bubble-who">{who}</div>}
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
        <article key={p.id} className="approval card" data-testid={`approval-${p.kind}`} data-id={p.id}>
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
        <div className="modal">
          <div className="dialog dialog-danger" role="dialog" aria-label="Confirm undo">
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
        </div>
      )}
      <div ref={end} />
    </div>
  );
}
