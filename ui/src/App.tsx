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

export const TABS = [
  ["story", "What happened"],
  ["messages", "Message stream"],
  ["claims", "Claim map"],
  ["decisions", "Decision log"],
  ["merges", "Merge queue"],
  ["cost", "Cost meter"],
  ["snapshots", "Snapshot timeline"],
  ["audit", "Audit log"],
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
  return (
    <div className="app">
      <header className="top">
        <div className="title">
          <h1>Anchor</h1>
          {ov && (
            <span className={`badge badge-${ov.mode === "live" && ov.running ? "live" : "muted"}`}>
              {ov.mode === "live" ? (ov.running ? "LIVE · team running" : "run finished") : "inspecting"}
            </span>
          )}
          {ov && <span className="ws" title={ov.workspace}>{ov.workspace}</span>}
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
      />
      <div className="layout">
        <aside className="side">
          {ov && <Approvals pending={ov.pending} colors={colors} onAnswered={refreshOverview} />}
          {ov && <Roster roster={ov.roster} colors={colors} />}
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
            {tab === "claims" && <Claims version={version} colors={colors} />}
            {tab === "decisions" && <Decisions version={version} colors={colors} overview={ov} />}
            {tab === "merges" && <MergeQueue version={version} colors={colors} focus={focus?.merge} />}
            {tab === "cost" && <Cost version={version} colors={colors} />}
            {tab === "snapshots" && <Snapshots version={version} colors={colors} focus={focus?.snapshot} />}
            {tab === "audit" && <Audit events={events} overview={ov} />}
          </section>
        </main>
      </div>
    </div>
  );
}
