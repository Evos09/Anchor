import { useEffect, useRef, useState, type ReactNode } from "react";
import { get, post } from "../api";
import type { AppState, InferenceStatus, Overview, PullState, TestsInfo } from "../types";
import { Meters, useInference } from "./Inference";
import { ErrorLine } from "../util";
import * as I from "../icons";

export type Section = "general" | "models" | "appearance" | "projects" | "about";

const SECTIONS: { id: Section; label: string }[] = [
  { id: "general", label: "General" },
  { id: "models", label: "Models" },
  { id: "appearance", label: "Appearance" },
  { id: "projects", label: "Projects" },
  { id: "about", label: "About" },
];

type Theme = "dark" | "light" | "system";

// The theme is a per-viewer convenience, so browser storage is enough; it
// may be unavailable (blocked storage), and then the default applies.
function loadTheme(): Theme {
  try {
    const t = localStorage.getItem("anchor.theme");
    if (t === "light" || t === "system" || t === "dark") return t;
  } catch {
    /* storage unavailable */
  }
  return "dark";
}

/** Apply the saved theme to the page (`<html data-theme>`). */
export function applyTheme(t: Theme = loadTheme()) {
  document.documentElement.dataset["theme"] = t;
}

function saveTheme(t: Theme) {
  try {
    localStorage.setItem("anchor.theme", t);
  } catch {
    /* storage unavailable: the choice lasts until the window closes */
  }
  applyTheme(t);
}

/** The settings screen: its own navigation on the left, grouped cards. */
export function Settings({
  app,
  overview,
  section,
  onSection,
  onClose,
  onChanged,
  onOpen,
}: {
  app: AppState;
  overview: Overview | null;
  section: Section;
  onSection: (s: Section) => void;
  onClose: () => void;
  onChanged: () => void;
  onOpen: (path: string) => void;
}) {
  return (
    <div className="settings">
      <nav className="settings-nav" aria-label="Settings sections">
        <p className="settings-nav-title">Settings</p>
        {SECTIONS.map((s) => (
          <button
            key={s.id}
            className={section === s.id ? "side-item side-on" : "side-item"}
            aria-current={section === s.id ? "page" : undefined}
            onClick={() => onSection(s.id)}
          >
            {s.label}
          </button>
        ))}
      </nav>
      <main className="settings-main">
        <button className="icon-btn settings-close" aria-label="Close settings" onClick={onClose}>
          <I.Close />
        </button>
        <div className="settings-page">
          {section === "general" && <General app={app} overview={overview} onChanged={onChanged} />}
          {section === "models" && <Models app={app} onChanged={onChanged} />}
          {section === "appearance" && <Appearance />}
          {section === "projects" && <Projects app={app} onOpen={onOpen} />}
          {section === "about" && <About app={app} />}
        </div>
      </main>
    </div>
  );
}

function Page({ title, intro, children }: { title: string; intro: string; children: ReactNode }) {
  return (
    <>
      <h1>{title}</h1>
      <p className="muted settings-intro">{intro}</p>
      {children}
    </>
  );
}

function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="set-group">
      <h2>{title}</h2>
      <div className="set-card">{children}</div>
    </section>
  );
}

function Row({ title, desc, children }: { title: string; desc: ReactNode; children?: ReactNode }) {
  return (
    <div className="set-row">
      <div className="set-text">
        <div className="set-title">{title}</div>
        <div className="set-desc">{desc}</div>
      </div>
      {children && <div className="set-control">{children}</div>}
    </div>
  );
}

/** Open one of the few pages the backend allows (Releases, Ollama). */
export function openUrl(url: string) {
  post("/api/app/open_url", { url }).catch(() => window.open(url, "_blank", "noopener"));
}

function mb(n: number): string {
  return n >= 1e9 ? `${(n / 1e9).toFixed(1)} GB` : `${Math.round(n / 1e6)} MB`;
}

/** The two recommended local models, with Download buttons. */
function Recommended({ s, onDone }: { s: InferenceStatus; onDone: () => void }) {
  const [pulls, setPulls] = useState<PullState[]>([]);
  const [error, setError] = useState<string | null>(null);
  const active = pulls.some((p) => !p.done);
  const done = useRef(onDone);
  done.current = onDone;
  useEffect(() => {
    let live = true;
    const load = () =>
      get<{ pulls: PullState[] }>("/api/app/pulls").then(
        (v) => {
          if (!live) return;
          setPulls((old) => {
            if (old.some((p) => !p.done) && v.pulls.every((p) => p.done)) done.current();
            return v.pulls;
          });
        },
        () => undefined,
      );
    load();
    const t = window.setInterval(load, active ? 700 : 5000);
    return () => {
      live = false;
      window.clearInterval(t);
    };
  }, [active]);
  if (!s.local.running) {
    return (
      <Row
        title="Get Ollama"
        desc="Ollama runs AI models on this computer. Install it, start it, then come back here to download models with one click."
      >
        <button className="btn btn-small" onClick={() => openUrl("https://ollama.com/download")}>
          Get Ollama
        </button>
      </Row>
    );
  }
  const start = (model: string) =>
    post("/api/app/pull", { model }).then(
      () => (setError(null), get<{ pulls: PullState[] }>("/api/app/pulls").then((v) => setPulls(v.pulls))),
      (e) => setError(String(e)),
    );
  const cancel = (model: string) => post("/api/app/pull_cancel", { model }).catch((e) => setError(String(e)));
  return (
    <div data-testid="recommended">
      <Row
        title="Recommended models"
        desc="A builder and a checker from two different families, so Builder + checker works with nothing leaving this computer."
      />
      {error && (
        <div className="set-row set-row-wide">
          <ErrorLine error={error} />
        </div>
      )}
      {s.local.recommended.map((m) => {
        const p = pulls.find((x) => x.model === m.model);
        const ready = s.local.ready.some((r) => r.model === m.model);
        const pct = p && p.total > 0 ? Math.min(100, Math.round((100 * p.completed) / p.total)) : 0;
        return (
          <Row
            key={m.model}
            title={`${m.label} (${m.role})`}
            desc={
              <>
                <code>{m.model}</code> · family {m.family} · license {m.license} · about {mb(m.size)}
                {p && !p.done && (
                  <span className="pull">
                    <span
                      className="pull-bar"
                      role="meter"
                      aria-label={`Downloading ${m.model}`}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={pct}
                    >
                      <span style={{ width: `${pct}%` }} />
                    </span>
                    {p.total > 0 ? `${mb(p.completed)} of ${mb(p.total)}` : p.status}
                  </span>
                )}
                {p?.done && p.error && p.error !== "cancelled" && <span className="fail"> · {p.error}</span>}
              </>
            }
          >
            {ready ? (
              <Pill kind="ok">Ready</Pill>
            ) : p && !p.done ? (
              <button className="btn btn-small" onClick={() => cancel(m.model)}>
                Cancel
              </button>
            ) : (
              <button className="btn btn-small btn-primary" onClick={() => start(m.model)}>
                Download
              </button>
            )}
          </Row>
        );
      })}
    </div>
  );
}

function Pill({ kind, children }: { kind: "ok" | "warn" | "fail" | "muted"; children: ReactNode }) {
  return <span className={`pill pill-${kind}`}>{children}</span>;
}

function General({ app, overview, onChanged }: { app: AppState; overview: Overview | null; onChanged: () => void }) {
  const chain = overview?.chain;
  return (
    <Page title="General" intro="How the agent works in your projects. Anchor's safety rules are always on.">
      <Group title="Safety">
        <Row
          title="Approvals"
          desc="The agent asks you before it changes a file or runs a command. This can't be turned off: it is what makes the agent safe to leave working."
        >
          <Pill kind="ok">Always ask</Pill>
        </Row>
        <Row title="Sandbox" desc="Commands run sandboxed, limited to the project folder.">
          <Pill kind="ok">On</Pill>
        </Row>
        <Row title="Undo" desc="A snapshot is taken before every turn that changes files, so any turn can be undone.">
          <Pill kind="ok">On</Pill>
        </Row>
        <Row
          title="Audit log"
          desc="Every action is recorded in a hash-chained log, checked each time it is read (Audit log, top right)."
        >
          {!chain ? (
            <Pill kind="muted">Open a project</Pill>
          ) : chain.error ? (
            <Pill kind="fail">Broken</Pill>
          ) : (
            <Pill kind="ok">Verified ({chain.verified_entries})</Pill>
          )}
        </Row>
      </Group>
      <Group title="Tasks">
        <Row title="Sending" desc="Enter sends a task; Shift+Enter starts a new line." />
        <Row title="One task at a time" desc="Stop the running task, or wait for it, before starting another." />
        {app.inference?.source === "file" ? (
          <Row title="Models" desc={app.models.ok ? `Using ${app.models.path}` : `Not ready: ${app.models.error}`}>
            <Pill kind={app.models.ok ? "ok" : "warn"}>{app.models.ok ? "Ready" : "Needs a fix"}</Pill>
          </Row>
        ) : (
          <Row title="Models" desc="Chosen for each task: local, your API key, or the free tier (Settings → Models)." />
        )}
      </Group>
      <Group title="Updates">
        <Row
          title="Check for updates"
          desc={`Asks GitHub for the latest Anchor version a few times a day and shows a Download button when there is a newer one. It sends nothing about you or your projects.${app.version ? ` This is Anchor ${app.version}.` : ""}`}
        >
          <input
            type="checkbox"
            aria-label="Check for updates"
            checked={app.inference?.check_updates ?? true}
            onChange={(e) => post("/api/app/inference", { check_updates: e.target.checked }).then(onChanged, () => undefined)}
          />
        </Row>
      </Group>
    </Page>
  );
}

function Models({ app, onChanged }: { app: AppState; onChanged: () => void }) {
  const [s, refresh, loadError] = useInference(15000);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const act = async (path: string, body: unknown, ok?: string) => {
    try {
      const r = await post<{ models?: string[] }>(path, body);
      setError(null);
      setNote(ok ?? null);
      refresh();
      onChanged();
      return r;
    } catch (e) {
      setError(String(e));
      setNote(null);
      return null;
    }
  };
  if (!s) {
    return (
      <Page title="Models" intro="Which AI models do the work.">
        <p className="muted">{loadError ?? "Checking what is available…"}</p>
      </Page>
    );
  }
  const routed = s.source === "routed";
  return (
    <Page
      title="Models"
      intro="Anchor can run a task three ways: on this computer (Ollama), with your own API key, or on the free tier. The reviewer is always a different model family from the executor, so a model never reviews its own work."
    >
      {(error || note) && (
        <div className="set-row set-row-wide">
          <ErrorLine error={error} />
          {note && !error && <span className="ok">{note}</span>}
        </div>
      )}
      <Group title="Two AIs">
        <Row
          title="How the AIs work"
          desc="Builder + checker: one model makes the change, a second model from another family reviews it and sends it back with notes until it accepts. Two builders: both work at once in separate copies, split the work through messages, and you confirm each merge."
        >
          <select
            aria-label="How the AIs work"
            value={app.inference?.collab ?? "pair"}
            onChange={(e) => act("/api/app/inference", { collab: e.target.value }, "Saved.")}
          >
            <option value="single">One AI</option>
            <option value="pair">Builder + checker</option>
            <option value="team">Two builders</option>
          </select>
        </Row>
        <Row
          title="Review rounds"
          desc="How many times the checker may send the change back (Builder + checker). Every round's changes still need your OK."
        >
          <select
            aria-label="Review rounds"
            value={String(app.inference?.pair_rounds ?? 2)}
            onChange={(e) => act("/api/app/inference", { pair_rounds: Number(e.target.value) }, "Saved.")}
          >
            {[1, 2, 3, 4, 5].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </Row>
      </Group>
      <Group title="How tasks choose">
        <Row
          title="Preference"
          desc="Automatic sends simple tasks to local or free models and bigger ones to your key. Start any task with /local, /byok or /free to choose for that task only."
        >
          <select
            aria-label="Preference"
            value={s.preference}
            disabled={!routed}
            onChange={(e) => act("/api/app/inference", { preference: e.target.value }, "Saved.")}
          >
            <option value="auto">Automatic</option>
            <option value="prefer_local">Local first</option>
            <option value="prefer_byok">My API key first</option>
            <option value="free_only">Free tier only</option>
          </select>
        </Row>
        {app.project && (
          <Row
            title="This project"
            desc="Pin the open project to one way, e.g. a private repository to local only. A pinned project never uses another."
          >
            <select
              aria-label="Project pin"
              value={s.pin ?? ""}
              disabled={!routed}
              onChange={(e) => act("/api/app/pin", { path: e.target.value || null }, "Saved.")}
            >
              <option value="">Not pinned</option>
              <option value="C">Local only</option>
              <option value="B">My API key only</option>
              <option value="A">Free tier only</option>
            </select>
          </Row>
        )}
      </Group>

      <Group title="Local — nothing leaves your machine">
        <Row
          title="Ollama"
          desc={
            s.local.running
              ? `Local inference active (Ollama ${s.local.version ?? ""} on port ${s.local.port}).`
              : `Not found on port ${s.local.port}. Install Ollama from ollama.com and start it; Anchor finds it by itself.`
          }
        >
          <Pill kind={s.local.running ? "ok" : "muted"}>{s.local.running ? "Running" : "Not running"}</Pill>
        </Row>
        <PortRow port={s.local.port} onSave={(p) => act("/api/app/inference", { ollama_port: p }, "Saved.")} />
        <Recommended s={s} onDone={refresh} />
        {s.local.ready.map((m) => (
          <Row
            key={m.model}
            title={m.label}
            desc={
              <>
                <code>{m.model}</code> · family {m.family}
                {m.license && (
                  <>
                    {" "}
                    · license {m.license.id}
                    {!m.license.commercial && " (non-commercial)"}
                    {m.license.notes && <> — {m.license.notes}</>}
                  </>
                )}
              </>
            }
          >
            {s.preferred.C === m.model ? (
              <Pill kind="ok">Executor</Pill>
            ) : (
              <button
                className="btn btn-small"
                onClick={() => act("/api/app/inference", { preferred: { path: "C", model: m.model } }, "Saved.")}
              >
                Use as executor
              </button>
            )}
          </Row>
        ))}
        {s.local.needs_family.map((m) => (
          <FamilyRow
            key={m.model}
            model={m.model}
            hint={m.hint}
            onSave={(f) => act("/api/app/inference", { local_family: { model: m.model, family: f } }, "Saved.")}
          />
        ))}
      </Group>

      <Group title="Your API keys">
        <Row
          title="Where keys live"
          desc="In your system keychain. A key goes only to its own provider — never to Anchor, never into a log. Removing it wipes it from the keychain."
        />
        {s.byok.map((p) => (
          <KeyRow key={p.id} p={p} act={act} />
        ))}
      </Group>

      <Group title="Free tier">
        {!s.free.offered ? (
          <Row
            title="Not offered yet"
            desc="Anchor offers a free tier only through providers whose terms allow it, and none has passed that check yet. Use a local model or your own key meanwhile."
          >
            <Pill kind="muted">Unavailable</Pill>
          </Row>
        ) : !s.free.started ? (
          <Row
            title="Start free — no signup"
            desc={
              s.free.state.state === "available"
                ? "A capped free tier for trying Anchor."
                : s.free.state.reason
            }
          >
            <button className="btn btn-primary" onClick={() => act("/api/app/free_start", {}, "The free tier is on.")}>
              Start free
            </button>
          </Row>
        ) : (
          <>
            <Row
              title="Free tier"
              desc={s.free.state.state === "available" ? "Ready." : s.free.state.reason}
            >
              <Pill kind={s.free.state.state === "available" ? "ok" : "warn"}>
                {s.free.state.state === "available" ? "On" : "Paused"}
              </Pill>
            </Row>
            <div className="set-row set-row-wide">
              <Meters s={s} />
            </div>
          </>
        )}
      </Group>

      <Group title="Advanced: a models file">
        <Row
          title="Use a models file instead"
          desc="For full control: a roles file naming each model. When on, the choices above are not used."
        >
          <div className="segmented" role="radiogroup" aria-label="Model source">
            {(["routed", "file"] as const).map((src) => (
              <button
                key={src}
                role="radio"
                aria-checked={s.source === src}
                className={s.source === src ? "seg seg-on" : "seg"}
                onClick={() => act("/api/app/inference", { source: src }, "Saved.")}
              >
                {src === "routed" ? "Choose for me" : "Models file"}
              </button>
            ))}
          </div>
        </Row>
        {!routed && <ModelsFile app={app} onChanged={onChanged} />}
      </Group>
    </Page>
  );
}

function PortRow({ port, onSave }: { port: number; onSave: (p: number) => void }) {
  const [v, setV] = useState(String(port));
  const n = Number(v);
  const valid = Number.isInteger(n) && n > 0 && n < 65536;
  return (
    <Row title="Port" desc="Where Ollama listens (11434 unless you changed it).">
      <input
        className="small-input"
        aria-label="Ollama port"
        value={v}
        onChange={(e) => setV(e.target.value)}
        inputMode="numeric"
      />
      <button className="btn btn-small" disabled={!valid || n === port} onClick={() => onSave(n)}>
        Save
      </button>
    </Row>
  );
}

function FamilyRow({ model, hint, onSave }: { model: string; hint: string; onSave: (f: string) => void }) {
  const [v, setV] = useState("");
  return (
    <Row
      title={model}
      desc={`Anchor doesn't know this model's family yet${hint ? ` (Ollama says "${hint}")` : ""}. Name it so the reviewer can be a different family.`}
    >
      <input className="small-input" aria-label={`Family of ${model}`} value={v} onChange={(e) => setV(e.target.value)} />
      <button className="btn btn-small" disabled={!v.trim()} onClick={() => onSave(v.trim())}>
        Save
      </button>
    </Row>
  );
}

function KeyRow({
  p,
  act,
}: {
  p: InferenceStatus["byok"][number];
  act: (path: string, body: unknown, ok?: string) => Promise<{ models?: string[] } | null>;
}) {
  const [key, setKey] = useState("");
  const [models, setModels] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const add = async () => {
    setBusy(true);
    const r = await act("/api/app/key", { provider: p.id, key }, "Key checked and saved.");
    setBusy(false);
    if (r) {
      setKey("");
      setModels(r.models ?? []);
    }
  };
  const list = async () => {
    try {
      setModels((await get<{ models: string[] }>(`/api/app/key_models?provider=${encodeURIComponent(p.id)}`)).models);
    } catch {
      setModels([]);
    }
  };
  return (
    <div className="set-row set-row-wide" data-testid={`key-${p.id}`}>
      <div className="set-text">
        <div className="set-title">{p.name}</div>
        <div className="set-desc">
          {p.has_key
            ? p.problem
              ? `Key saved, but: ${p.problem}.`
              : `Key saved · model ${p.model}`
            : p.key_url
              ? <>Get a key at <code>{p.key_url}</code></>
              : "Paste a key to add it."}
        </div>
      </div>
      {!p.has_key ? (
        <>
          <input
            className="wide-input"
            type="password"
            autoComplete="off"
            aria-label={`${p.name} API key`}
            placeholder="Paste API key"
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <button className="btn btn-primary" disabled={!key.trim() || busy} onClick={add}>
            {busy ? "Checking…" : "Check and save"}
          </button>
        </>
      ) : (
        <>
          {models ? (
            <select
              aria-label={`${p.name} model`}
              value={p.model ?? ""}
              onChange={(e) => act("/api/app/key_model", { provider: p.id, model: e.target.value }, "Saved.")}
            >
              <option value="">Pick a model…</option>
              {models.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </select>
          ) : (
            <button className="btn btn-small" onClick={list}>
              {p.model ? "Change model" : "Pick a model"}
            </button>
          )}
          <button className="btn btn-small btn-danger" onClick={() => act("/api/app/key_remove", { provider: p.id }, "Key removed from the keychain.")}>
            Remove
          </button>
        </>
      )}
    </div>
  );
}

function ModelsFile({ app, onChanged }: { app: AppState; onChanged: () => void }) {
  const [path, setPath] = useState(app.models.path);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const save = async () => {
    try {
      await post("/api/app/models", { path });
      setError(null);
      setSaved(true);
      onChanged();
    } catch (e) {
      setError(String(e));
      setSaved(false);
    }
  };
  const roles = app.models.roles;
  return (
    <>
      <Row
        title="File"
        desc={
          app.models.ok ? (
            <span className="ok">Checked: ready to use.</span>
          ) : (
            <span className="fail">Can't be used yet: {app.models.error}</span>
          )
        }
      />
      <div className="set-row set-row-wide">
        <input
          className="wide-input"
          aria-label="Models file"
          value={path}
          onChange={(e) => {
            setPath(e.target.value);
            setSaved(false);
          }}
        />
        <button className="btn btn-primary" onClick={save}>
          Use this file
        </button>
      </div>
      {(error || saved) && (
        <div className="set-row set-row-wide">
          <ErrorLine error={error} />
          {saved && !error && <span className="ok">Saved.</span>}
        </div>
      )}
      <Row title="Executor" desc="Plans and makes the changes.">
        <code>{roles.executor ?? "—"}</code>
      </Row>
      <Row title="Reviewer" desc="Checks the executor's work before it counts.">
        <code>{roles.reviewer ?? "—"}</code>
      </Row>
    </>
  );
}

function Appearance() {
  const [theme, setTheme] = useState<Theme>(loadTheme());
  const pick = (t: Theme) => {
    setTheme(t);
    saveTheme(t);
  };
  return (
    <Page title="Appearance" intro="How Anchor looks on this computer.">
      <Group title="Theme">
        <Row title="Color theme" desc="System follows your computer's light or dark setting.">
          <div className="segmented" role="radiogroup" aria-label="Color theme">
            {(["dark", "light", "system"] as Theme[]).map((t) => (
              <button
                key={t}
                role="radio"
                aria-checked={theme === t}
                className={theme === t ? "seg seg-on" : "seg"}
                onClick={() => pick(t)}
              >
                {t[0]!.toUpperCase() + t.slice(1)}
              </button>
            ))}
          </div>
        </Row>
      </Group>
    </Page>
  );
}

function Projects({ app, onOpen }: { app: AppState; onOpen: (p: string) => void }) {
  return (
    <Page title="Projects" intro="Folders you have opened recently. Anchor keeps each project's history and snapshots.">
      <Group title="Recent">
        {!app.recent.length && <Row title="None yet" desc="Open a folder to start." />}
        {app.recent.map((r) => {
          const gone = app.missing.includes(r);
          return (
            <Row key={r} title={r.split(/[\\/]+/).filter(Boolean).pop() ?? r} desc={r}>
              {r === app.project ? (
                <Pill kind="ok">Open</Pill>
              ) : gone ? (
                <Pill kind="fail">Folder not found</Pill>
              ) : (
                <button className="btn btn-small" onClick={() => onOpen(r)} disabled={app.running}>
                  Open
                </button>
              )}
            </Row>
          );
        })}
      </Group>
      {app.project && <TestCommand project={app.project} />}
    </Page>
  );
}

/** Split a command line into its parts ("double quotes" keep spaces). */
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  const re = /"([^"]*)"|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(s))) out.push(m[1] ?? m[2] ?? "");
  return out;
}

function joinArgs(a: string[]): string {
  return a.map((x) => (/\s/.test(x) ? `"${x}"` : x)).join(" ");
}

/** The open project's test command: run on a copy of the project after
 * each change, so the checker knows whether the tests pass. */
function TestCommand({ project }: { project: string }) {
  const [t, setT] = useState<TestsInfo | null>(null);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const load = () =>
    get<TestsInfo>("/api/app/tests").then(
      (v) => {
        setT(v);
        setText(joinArgs(v.setup?.argv ?? v.suggested ?? []));
      },
      (e) => setError(String(e)),
    );
  useEffect(() => {
    void load();
  }, [project]);
  const save = (argv: string[] | null) =>
    post("/api/app/tests", { argv }).then(
      () => (setError(null), setNote(argv ? "Saved." : "Turned off."), load()),
      (e) => (setError(String(e)), setNote(null)),
    );
  return (
    <Group title="Tests of the open project">
      <Row
        title="Test command"
        desc={
          <>
            Runs after each change on a copy of the project, without network, so the checker sees whether your tests
            pass. A change that makes them fail goes back to the builder.{" "}
            {t && !t.setup && (t.suggested ? `Suggested for ${t.why}.` : `Anchor found ${t.why}.`)}
          </>
        }
      >
        {t?.setup ? <Pill kind="ok">On</Pill> : <Pill kind="muted">Off</Pill>}
      </Row>
      <div className="set-row set-row-wide">
        <input
          className="wide-input"
          aria-label="Test command"
          placeholder="e.g. npm test"
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        <button className="btn btn-small btn-primary" disabled={!splitArgs(text).length} onClick={() => save(splitArgs(text))}>
          Save
        </button>
        {t?.setup && (
          <button className="btn btn-small" onClick={() => save(null)}>
            Turn off
          </button>
        )}
        <ErrorLine error={error} />
        {note && !error && <span className="ok">{note}</span>}
      </div>
    </Group>
  );
}

function About({ app }: { app: AppState }) {
  return (
    <Page title="About" intro="Anchor: a coding agent that asks first, sandboxes what it runs, and lets you undo.">
      <Group title="This app">
        <Row title="Platform" desc={app.platform} />
        <Row
          title="Unsigned preview"
          desc="This build isn't code-signed yet, so Windows and macOS warn when you install it."
        />
        <Row
          title="Privacy"
          desc="No telemetry. With a local model your code never leaves this computer. With your own key it goes straight to that provider. On the free tier it passes through Anchor's proxy, which keeps only usage counts, never content."
        />
      </Group>
    </Page>
  );
}
