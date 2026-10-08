import { useState, type ReactNode } from "react";
import { post } from "../api";
import type { AppState, Overview } from "../types";
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
          {section === "general" && <General app={app} overview={overview} />}
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

function Pill({ kind, children }: { kind: "ok" | "warn" | "fail" | "muted"; children: ReactNode }) {
  return <span className={`pill pill-${kind}`}>{children}</span>;
}

function General({ app, overview }: { app: AppState; overview: Overview | null }) {
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
        <Row title="Models" desc={app.models.ok ? `Using ${app.models.path}` : `Not ready: ${app.models.error}`}>
          <Pill kind={app.models.ok ? "ok" : "warn"}>{app.models.ok ? "Ready" : "Needs a fix"}</Pill>
        </Row>
      </Group>
    </Page>
  );
}

function Models({ app, onChanged }: { app: AppState; onChanged: () => void }) {
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
    <Page
      title="Models"
      intro="Which AI models do the work. The reviewer must be a different model family from the executor, so a model never reviews its own work."
    >
      <Group title="Models file">
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
      </Group>
      <Group title="Roles">
        <Row title="Executor" desc="Plans and makes the changes.">
          <code>{roles.executor ?? "—"}</code>
        </Row>
        <Row title="Reviewer" desc="Checks the executor's work before it counts.">
          <code>{roles.reviewer ?? "—"}</code>
        </Row>
      </Group>
      <Group title="Local models">
        <Row
          title="Ollama"
          desc="The default file uses two free local models through Ollama. Install it from ollama.com, then run: ollama pull qwen2.5-coder:7b and ollama pull llama3.1:8b"
        />
      </Group>
    </Page>
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
    </Page>
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
        <Row title="Privacy" desc="No telemetry. Your code stays on this computer unless your models file names a cloud provider." />
      </Group>
    </Page>
  );
}
