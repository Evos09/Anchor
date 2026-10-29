import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, get, hasToken, q } from "./api";
import type { ClaimsView, Event, Overview } from "./types";
import { AgentChip, makeColors, usd } from "./util";
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
import { AgentSidecar } from "./panels/AgentSidecar";

export const TABS = [
  ["story", "What happened"],
  ["workbench", "Code workbench"],
  ["artifacts", "Artifacts & plans"],
  ["customizations", "Skills, hooks & MCP"],
  ["claims", "Claim map"],
  ["merges", "Merge queue"],
  ["messages", "Message stream"],
  ["decisions", "Decision log"],
  ["snapshots", "Snapshot timeline"],
  ["cost", "Cost meter"],
  ["audit", "Audit log"],
] as const;
export type Tab = (typeof TABS)[number][0];

const ACTIVITY_ITEMS: { id: Tab; icon: string; short: string; title: string }[] = [
  { id: "story", icon: "◈", short: "MISSION", title: "Mission Control & What Happened" },
  { id: "workbench", icon: "⌘", short: "CODE", title: "Code Workbench & File Editor" },
  { id: "artifacts", icon: "▤", short: "PLANS", title: "Artifacts & Implementation Plans Studio" },
  { id: "claims", icon: "◫", short: "CLAIMS", title: "Multi-Agent File Claim Map" },
  { id: "merges", icon: "⑃", short: "MERGES", title: "Merge Queue & Verification Evidence" },
  { id: "snapshots", icon: "⟲", short: "SNAPS", title: "Git-Backed Snapshot Timeline" },
  { id: "customizations", icon: "⚙", short: "SKILLS", title: "Skills, Lifecycle Hooks & MCP" },
];

function tabFromHash(): Tab {
  const h = window.location.hash.replace(/^#\/?/, "");
  return (TABS.find(([t]) => t === h)?.[0] ?? "story") as Tab;
}

export function App() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [claimsView, setClaimsView] = useState<ClaimsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTabState] = useState<Tab>(tabFromHash);
  const [focus, setFocus] = useState<{ merge?: number; snapshot?: string } | null>(null);
  const [selectedFile, setSelectedFile] = useState<string | null>("calc.py");
  const [workbenchOpen, setWorkbenchOpen] = useState(false);
  const [sidecarOpen, setSidecarOpen] = useState(true);
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
      const [ovData, claimsData] = await Promise.all([
        get<Overview>("/api/overview"),
        get<ClaimsView>("/api/claims").catch(() => null),
      ]);
      setOverview(ovData);
      if (claimsData) {
        setClaimsView(claimsData);
      }
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError && e.status === 401 ? "unauthorized" : String(e));
    }
  }, []);

  const syncEventsNow = useCallback(async () => {
    try {
      const page: { events: Event[]; more: boolean } = await get<{ events: Event[]; more: boolean }>(
        `/api/events${q({ after: lastSeq.current })}`,
      );
      if (page.events.length) {
        lastSeq.current = page.events[page.events.length - 1]!.seq;
        setEvents((old) => old.concat(page.events));
      }
      await refreshOverview();
    } catch {
      // background long-poll handles retries
    }
  }, [refreshOverview]);

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

  const workspaceFiles = useMemo(() => {
    if (!claimsView) return [];
    return claimsView.files.map((path) => {
      const holders = claimsView.claims
        .filter((c) => c.status === "active" && !c.lease_expired && (c.path === path || path.startsWith(`${c.path}/`)))
        .map((c) => c.agent ?? `agent-${c.agent_id}`);
      const conflict = claimsView.conflicts.some(
        (cf) => cf.path === path || (cf.paths && cf.paths.includes(path)),
      );
      return { path, holders, conflict };
    });
  }, [claimsView]);

  if (!hasToken() || error === "unauthorized") {
    return (
      <main className="gate">
        <h1>Anchor · Antigravity Desktop</h1>
        <p>
          This page needs its access token. Open the full URL that <code>anchor ui</code> or{" "}
          <code>anchor-desktop.exe</code> launched (it ends in <code>#t=…</code>).
        </p>
      </main>
    );
  }

  const ov = overview;

  return (
    <div className="app">
      {/* ZONE 0: TOP TELEMETRY HEADER */}
      <header className="top">
        <div className="title">
          <span className="brand-mark" aria-hidden="true">
            ■
          </span>
          <h1>Anchor</h1>
          <span className="brand-sub mono">ANTIGRAVITY IDE · v2.0</span>
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
            <button className="stat stat-plan" onClick={() => setTab("artifacts")} title="Reasoning CoT blocks recorded">
              <b>{ov.reasoning_steps ?? 0}</b> CoT
            </button>
            <button
              className={`stat ${ov.chain.error ? "stat-fail" : "stat-ok"}`}
              onClick={() => setTab("audit")}
              title="The audit log's hash chain, verified as it is read"
            >
              {ov.chain.error ? "audit chain BROKEN" : `audit chain verified (${ov.chain.verified_entries})`}
            </button>
            <button
              type="button"
              className={`stat ${sidecarOpen ? "stat-plan" : ""}`}
              onClick={() => setSidecarOpen((v) => !v)}
              title="Toggle Antigravity Agent Sidecar"
            >
              {sidecarOpen ? "✦ Sidecar ON" : "✦ Sidecar OFF"}
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

      {/* OMNI-COMMAND BAR */}
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

      {/* 4-ZONE ANTIGRAVITY WORKBENCH SHELL */}
      <div
        className={[
          "ide-shell",
          workbenchOpen ? "ide-shell-split" : "",
          sidecarOpen ? "ide-shell-sidecar" : "ide-shell-nosidecar",
        ]
          .filter(Boolean)
          .join(" ")}
      >
        {/* ZONE 1: FAR-LEFT ACTIVITY RAIL */}
        <nav className="activity-bar" aria-label="Antigravity Activity Rail">
          <div className="activity-top">
            {ACTIVITY_ITEMS.map((item) => (
              <button
                key={item.id}
                type="button"
                className={tab === item.id ? "act-btn act-btn-on" : "act-btn"}
                title={item.title}
                onClick={() => setTab(item.id)}
              >
                <span className="act-icon" aria-hidden="true">
                  {item.icon}
                </span>
                <span className="act-label">{item.short}</span>
              </button>
            ))}
          </div>
          <div className="activity-bottom">
            <button
              type="button"
              className={workbenchOpen ? "act-btn act-btn-on" : "act-btn"}
              title="Toggle Split Code Inspector"
              onClick={() => setWorkbenchOpen((v) => !v)}
            >
              <span className="act-icon" aria-hidden="true">
                ⊞
              </span>
              <span className="act-label">SPLIT</span>
            </button>
            <button
              type="button"
              className={sidecarOpen ? "act-btn act-btn-on" : "act-btn"}
              title="Toggle Agent Sidecar"
              onClick={() => setSidecarOpen((v) => !v)}
            >
              <span className="act-icon" aria-hidden="true">
                ✦
              </span>
              <span className="act-label">AGENT</span>
            </button>
          </div>
        </nav>

        {/* ZONE 2: LEFT WORKSPACE EXPLORER & ROSTER SIDEBAR */}
        <aside className="side">
          {ov && <Approvals pending={ov.pending} colors={colors} onAnswered={refreshOverview} />}

          <section className="card" aria-label="Workspace Explorer">
            <div className="wb-section-head">
              <span>WORKSPACE EXPLORER</span>
              <span className="mono">{workspaceFiles.length} files</span>
            </div>
            <ul className="wb-filelist" style={{ maxHeight: "12rem" }}>
              {workspaceFiles.map((f) => (
                <li key={f.path}>
                  <button
                    type="button"
                    className={
                      selectedFile === f.path && tab === "workbench" ? "wb-filebtn wb-filebtn-on" : "wb-filebtn"
                    }
                    onClick={() => {
                      setSelectedFile(f.path);
                      setTab("workbench");
                    }}
                  >
                    <div className="row" style={{ justifyContent: "space-between" }}>
                      <span className="mono">{f.path}</span>
                      <span className="row">
                        {f.conflict && <span className="badge badge-fail">!</span>}
                        {f.holders.map((h) => (
                          <AgentChip key={h} name={h} color={colors(h)} />
                        ))}
                      </span>
                    </div>
                  </button>
                </li>
              ))}
              {!workspaceFiles.length && <li className="small muted">Scanning workspace tree…</li>}
            </ul>
          </section>

          {ov && <Roster roster={ov.roster} colors={colors} />}

          {ov && (
            <section className="card" aria-label="Harness Kernel Telemetry">
              <div className="wb-section-head">
                <span>CORDIS KERNEL</span>
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

        {/* ZONE 3: CENTER STAGE WORKBENCH & TELEMETRY */}
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
                onOpenTab={setTab}
                onInspectFile={(p) => {
                  setSelectedFile(p);
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

        {/* OPTIONAL SPLIT CODE INSPECTOR DRAWER */}
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

        {/* ZONE 4: RIGHT ANTIGRAVITY AGENT SIDECAR */}
        {sidecarOpen && (
          <AgentSidecar
            overview={ov}
            events={events}
            selectedFile={selectedFile}
            colors={colors}
            onDispatched={syncEventsNow}
            onOpenTab={setTab}
          />
        )}
      </div>
    </div>
  );
}
