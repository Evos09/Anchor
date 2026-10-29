import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, get, hasToken, q } from "./api";
import type { Event, Overview } from "./types";
import { makeColors, usd } from "./util";
import { Approvals } from "./panels/Approvals";
import { Roster } from "./panels/Roster";
import { Story } from "./panels/Story";
import { Claims } from "./panels/Claims";
import { Messages } from "./panels/Messages";
import { Decisions } from "./panels/Decisions";
import { MergeQueue } from "./panels/MergeQueue";
import { Cost } from "./panels/Cost";
import { Snapshots } from "./panels/Snapshots";
import { Audit } from "./panels/Audit";
import { CommandBar } from "./panels/CommandBar";
import { CodeWorkbench } from "./panels/CodeWorkbench";
import { ArtifactsPanel } from "./panels/ArtifactsPanel";
import { CustomizationsPanel } from "./panels/CustomizationsPanel";

export const TABS = [
  ["story", "What happened"],
  ["messages", "Message stream"],
  ["claims", "Claim map"],
  ["decisions", "Decision log"],
  ["merges", "Merge queue"],
  ["cost", "Cost meter"],
  ["snapshots", "Snapshot timeline"],
  ["audit", "Audit log"],
  ["workbench", "Code workbench"],
  ["artifacts", "Artifacts & plans"],
  ["customizations", "Skills, hooks & MCP"],
] as const;
export type Tab = (typeof TABS)[number][0];

function tabFromHash(): Tab {
  const h = window.location.hash.replace(/^#\/?/, "");
  return (TABS.find(([t]) => t === h)?.[0] ?? "story") as Tab;
}

export function App() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTabState] = useState<Tab>(tabFromHash);
  const [focus, setFocus] = useState<{ merge?: number; snapshot?: string } | null>(null);
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [workbenchOpen, setWorkbenchOpen] = useState(false);
  const lastSeq = useRef(-1);

  const setTab = useCallback((t: Tab) => {
    setTabState(t);
    history.replaceState(null, "", `#/${t}`);
  }, []);

  useEffect(() => {
    const onHash = () => setTabState(tabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const refreshOverview = useCallback(async () => {
    try {
      setOverview(await get<Overview>("/api/overview"));
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError && e.status === 401 ? "unauthorized" : String(e));
    }
  }, []);

  // The live feed: a long poll on new audit entries.
  useEffect(() => {
    if (!hasToken()) return;
    let stop = false;
    (async () => {
      await refreshOverview();
      let wait: number | undefined = undefined;
      while (!stop) {
        try {
          const page: { events: Event[]; more: boolean } = await get<{ events: Event[]; more: boolean }>(
            `/api/events${q({ after: lastSeq.current, wait })}`,
          );
          // Catch up at once, then long-poll.
          wait = page.more ? undefined : 1;
          if (page.events.length) {
            lastSeq.current = page.events[page.events.length - 1]!.seq;
            setEvents((old) => old.concat(page.events));
          }
          await refreshOverview();
          if (page.more) continue;
        } catch (e) {
          setError(e instanceof ApiError && e.status === 401 ? "unauthorized" : "connection lost; retrying");
          await new Promise((r) => setTimeout(r, 2000));
        }
      }
    })();
    return () => {
      stop = true;
    };
  }, [refreshOverview]);

  const colors = useMemo(() => makeColors((overview?.roster ?? []).map((r) => r.name)), [overview?.roster]);
  const version = overview?.last_seq ?? -1;

  if (!hasToken() || error === "unauthorized") {
    return (
      <main className="gate">
        <h1>Anchor dashboard</h1>
        <p>
          This page needs its access token. Open the full URL that <code>anchor ui</code> or{" "}
          <code>anchor team run --ui</code> printed (it ends in <code>#t=…</code>).
        </p>
      </main>
    );
  }

  const ov = overview;
  const todos = ov?.todos ?? [];

  return (
    <div className="app">
      <header className="top">
        <div className="title">
          <span className="brand-mark" aria-hidden="true">
            ■
          </span>
          <h1>Anchor</h1>
          {ov && (
            <span className={`badge badge-${ov.mode === "live" && ov.running ? "live" : "muted"}`}>
              {ov.mode === "live" ? (ov.running ? "LIVE · team running" : "run finished") : "inspecting"}
            </span>
          )}
          {ov && (
            <span className="ws" title={ov.workspace}>
              {ov.workspace}
            </span>
          )}
        </div>
        {ov && (
          <div className="stats" aria-label="summary">
            <button className="stat" onClick={() => setTab("cost")}>
              <b>{usd(ov.cost_total.spend_usd)}</b> spent
            </button>
            <button className={`stat ${ov.failures ? "stat-fail" : ""}`} onClick={() => setTab("story")}>
              <b>{ov.failures}</b> failure{ov.failures === 1 ? "" : "s"}
            </button>
            <button className={`stat ${ov.pending.length ? "stat-warn" : ""}`}>
              <b>{ov.pending.length}</b> awaiting you
            </button>
            {(ov.reasoning_steps ?? 0) > 0 && (
              <button className="stat stat-plan" onClick={() => setTab("story")} title="Reasoning CoT blocks recorded">
                <b>{ov.reasoning_steps}</b> CoT
              </button>
            )}
            <button
              className={`stat ${ov.chain.error ? "stat-fail" : "stat-ok"}`}
              onClick={() => setTab("audit")}
              title="The audit log's hash chain, verified as it is read"
            >
              {ov.chain.error ? "audit chain BROKEN" : `audit chain verified (${ov.chain.verified_entries})`}
            </button>
          </div>
        )}
      </header>
      {error && error !== "unauthorized" && (
        <div className="banner banner-warn" role="status">
          {error}
        </div>
      )}
      {ov?.chain.error && (
        <div className="banner banner-fail" role="alert">
          The audit log failed verification: {ov.chain.error}. Entries after this point are not shown.
        </div>
      )}
      {ov && ov.cost_discrepancies > 0 && (
        <div className="banner banner-fail" role="alert">
          {ov.cost_discrepancies} cost discrepanc{ov.cost_discrepancies === 1 ? "y" : "ies"}: reported and recomputed
          costs disagree.{" "}
          <button className="link" onClick={() => setTab("cost")}>
            See the cost meter
          </button>
        </div>
      )}
      <CommandBar
        overview={ov}
        onTab={setTab}
        onRollback={(r) => {
          setFocus({ snapshot: r });
          setTab("snapshots");
        }}
        onToggleWorkbench={() => setWorkbenchOpen((v) => !v)}
        workbenchOpen={workbenchOpen}
      />
      <div className={workbenchOpen ? "layout layout-split" : "layout"}>
        <aside className="side">
          {ov && <Approvals pending={ov.pending} colors={colors} onAnswered={refreshOverview} />}
          {ov && <Roster roster={ov.roster} colors={colors} />}
          {todos.length > 0 && (
            <section className="card" aria-label="Task Checklist">
              <div className="wb-section-head">
                <span>TASK CHECKLIST (TODOWRITE)</span>
                <span className="mono">{todos.length}</span>
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
            </section>
          )}
          {ov && (
            <section className="card" aria-label="Harness Kernel Telemetry">
              <div className="wb-section-head">
                <span>HARNESS KERNEL</span>
                <span className="mono muted">L1/L2</span>
              </div>
              <div className="kernel-stats small mono">
                <div>
                  <span>Reasoning blocks</span>
                  <b>{ov.reasoning_steps ?? 0}</b>
                </div>
                <div>
                  <span>Subagents spawned</span>
                  <b>{ov.subagents_spawned ?? 0}</b>
                </div>
                <div>
                  <span>Hooks executed</span>
                  <b>{ov.hooks_run ?? 0}</b>
                </div>
                <div>
                  <span>Snapshots store</span>
                  <b>{ov.store.available ? "ONLINE" : "LOCKED"}</b>
                </div>
              </div>
            </section>
          )}
        </aside>
        <main className="main">
          <nav className="tabs" role="tablist" aria-label="Panels">
            {TABS.map(([t, label]) => (
              <button
                key={t}
                role="tab"
                aria-selected={tab === t}
                className={tab === t ? "tab tab-on" : "tab"}
                onClick={() => setTab(t)}
              >
                {label}
              </button>
            ))}
          </nav>
          <section className="panel" role="tabpanel" aria-label={TABS.find(([t]) => t === tab)?.[1]}>
            {tab === "story" && (
              <Story
                events={events}
                colors={colors}
                agents={(ov?.roster ?? []).map((r) => r.name)}
                onMerge={(m) => {
                  setFocus({ merge: m });
                  setTab("merges");
                }}
              />
            )}
            {tab === "messages" && (
              <Messages
                version={version}
                colors={colors}
                onMerge={(m) => {
                  setFocus({ merge: m });
                  setTab("merges");
                }}
              />
            )}
            {tab === "claims" && (
              <Claims
                version={version}
                colors={colors}
                onInspectFile={(p) => {
                  setSelectedFile(p);
                  setWorkbenchOpen(true);
                }}
              />
            )}
            {tab === "decisions" && <Decisions version={version} colors={colors} overview={ov} />}
            {tab === "merges" && <MergeQueue version={version} colors={colors} focus={focus?.merge} />}
            {tab === "cost" && <Cost version={version} colors={colors} />}
            {tab === "snapshots" && <Snapshots version={version} colors={colors} focus={focus?.snapshot} />}
            {tab === "audit" && <Audit events={events} overview={ov} />}
            {tab === "workbench" && (
              <CodeWorkbench
                version={version}
                colors={colors}
                selectedFile={selectedFile}
                onSelectFile={setSelectedFile}
              />
            )}
            {tab === "artifacts" && <ArtifactsPanel version={version} />}
            {tab === "customizations" && <CustomizationsPanel version={version} />}
          </section>
        </main>
        {workbenchOpen && (
          <aside className="wb-drawer" aria-label="Code Workbench Drawer">
            <div className="wb-drawer-head">
              <strong>CODE INSPECTOR</strong>
              <div className="row">
                <button
                  type="button"
                  className="btn btn-small"
                  onClick={() => {
                    setWorkbenchOpen(false);
                    setTab("workbench");
                  }}
                >
                  Expand full
                </button>
                <button type="button" className="btn btn-small" onClick={() => setWorkbenchOpen(false)}>
                  ×
                </button>
              </div>
            </div>
            <CodeWorkbench
              version={version}
              colors={colors}
              selectedFile={selectedFile}
              onSelectFile={setSelectedFile}
              compact
            />
          </aside>
        )}
      </div>
    </div>
  );
}
