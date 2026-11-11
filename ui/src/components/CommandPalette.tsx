import { useEffect, useMemo, useRef, useState } from "react";
import type { Tab } from "../App";

export interface CommandItem {
  id: string;
  category: "Files" | "Navigation" | "Slash Commands" | "Actions" | "Terminal";
  title: string;
  subtitle?: string;
  icon: string;
  badge?: string;
  shortcut?: string;
  action: () => void;
}

export function CommandPalette({
  open,
  initialMode = "command",
  files,
  onClose,
  onSelectFile,
  onSelectTab,
  onRunCommand,
  onAskAgent,
  onToggleWorkbench,
  onToggleSidecar,
  onOpenShortcuts,
}: {
  open: boolean;
  initialMode?: "command" | "file";
  files: string[];
  onClose: () => void;
  onSelectFile: (path: string) => void;
  onSelectTab: (tab: Tab) => void;
  onRunCommand: (program: string, args: string[]) => void;
  onAskAgent: (prompt: string) => void;
  onToggleWorkbench: () => void;
  onToggleSidecar: () => void;
  onOpenShortcuts: () => void;
}) {
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (open) {
      setQuery(initialMode === "file" ? "" : "");
      setSelectedIndex(0);
      setTimeout(() => inputRef.current?.focus(), 30);
    }
  }, [open, initialMode]);

  const items = useMemo<CommandItem[]>(() => {
    const list: CommandItem[] = [];

    // 1. Files
    for (const f of files) {
      list.push({
        id: `file:${f}`,
        category: "Files",
        title: f,
        subtitle: "Open in Code Workbench",
        icon: "📄",
        badge: "FILE",
        action: () => {
          onSelectFile(f);
          onSelectTab("workbench");
          onClose();
        },
      });
    }

    // 2. Navigation Tabs
    const tabs: [Tab, string, string, string][] = [
      ["story", "Mission Control & Story", "View live events & approvals", "Ctrl+1"],
      ["workbench", "Code Workbench", "Multi-tab editor with FastCDC audit", "Ctrl+2"],
      ["artifacts", "Artifacts & Plans Studio", "Inspect Markdown plans & specs", "Ctrl+3"],
      ["terminal", "Sandboxed Task Runner", "Execute tests and tasks in workspace", "Ctrl+4"],
      ["claims", "Multi-Agent Claim Map", "Inspect active lock partitions", "Ctrl+5"],
      ["merges", "Merge Queue & Verification", "Inspect speculative merges", "Ctrl+6"],
      ["snapshots", "Snapshot Timeline", "FastCDC snapshots & rollback", "Ctrl+7"],
      ["customizations", "Skills & Hooks", "Rules, subagents, and MCP", "Ctrl+8"],
      ["cost", "Cost Meter", "Token spend & discrepancy checks", ""],
      ["audit", "Audit Log", "SHA-256 hash-chain verification", ""],
    ];

    for (const [t, title, subtitle, shortcut] of tabs) {
      list.push({
        id: `tab:${t}`,
        category: "Navigation",
        title: `Go to ${title}`,
        subtitle,
        icon: "⎘",
        badge: "ZONE",
        shortcut,
        action: () => {
          onSelectTab(t);
          onClose();
        },
      });
    }

    // 3. Slash Commands
    const slashCmds: [string, string, string][] = [
      ["/plan", "Draft Architecture & Implementation Plan", "Analyze workspace and author step-by-step implementation_plan.md"],
      ["/audit", "Verify Read-Before-Write & Boundary Guards", "Inspect workspace files for append-only invariants"],
      ["/team", "Prepare Multi-Agent File Claim Partition", "Partition files into non-overlapping edit claims"],
      ["/review", "Start Code Review against Active Diff", "Review recent changes for correctness and security"],
      ["/verify", "Run Full Verification Suite", "Execute all verifiers and sandboxed test suites"],
    ];

    for (const [cmd, title, prompt] of slashCmds) {
      list.push({
        id: `slash:${cmd}`,
        category: "Slash Commands",
        title: `${cmd} — ${title}`,
        subtitle: prompt,
        icon: "✦",
        badge: "AGENT",
        action: () => {
          onAskAgent(prompt);
          onClose();
        },
      });
    }

    // 4. Quick Terminal Presets
    const termPresets: [string, string, string, string[]][] = [
      ["Run Tests (Python unittest)", "Execute discover -v in workspace", "python", ["-m", "unittest", "discover", "-v"]],
      ["Compile & Syntax Check", "Execute compileall on workspace", "python", ["-m", "compileall", "-q", "."]],
      ["Git Status", "Inspect working tree porcelain status", "git", ["status", "--porcelain", "-b"]],
      ["Git Diff", "View uncommitted workspace diffs", "git", ["diff"]],
      ["Cargo Check", "Confined cargo syntax verification", "cargo", ["check"]],
      ["Cargo Test", "Run backend unit test suite", "cargo", ["test"]],
    ];

    for (const [title, subtitle, prog, args] of termPresets) {
      list.push({
        id: `term:${title}`,
        category: "Terminal",
        title,
        subtitle,
        icon: "▶",
        badge: "TASK",
        action: () => {
          onSelectTab("terminal");
          onRunCommand(prog, args);
          onClose();
        },
      });
    }

    // 5. Actions
    list.push({
      id: "act:split",
      category: "Actions",
      title: "Toggle Split Code Inspector",
      subtitle: "Show/hide secondary editor pane",
      icon: "⊞",
      badge: "LAYOUT",
      action: () => {
        onToggleWorkbench();
        onClose();
      },
    });

    list.push({
      id: "act:sidecar",
      category: "Actions",
      title: "Toggle Antigravity Agent Sidecar",
      subtitle: "Show/hide AI agent dock (Ctrl+J)",
      icon: "✦",
      badge: "LAYOUT",
      shortcut: "Ctrl+J",
      action: () => {
        onToggleSidecar();
        onClose();
      },
    });

    list.push({
      id: "act:shortcuts",
      category: "Actions",
      title: "Open Keyboard Shortcuts Map",
      subtitle: "View all Antigravity hotkeys (?)",
      icon: "⌨",
      badge: "HELP",
      shortcut: "?",
      action: () => {
        onOpenShortcuts();
        onClose();
      },
    });

    return list;
  }, [
    files,
    onSelectFile,
    onSelectTab,
    onAskAgent,
    onRunCommand,
    onToggleWorkbench,
    onToggleSidecar,
    onOpenShortcuts,
    onClose,
  ]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) {
      if (initialMode === "file") {
        return items.filter((i) => i.category === "Files");
      }
      return items;
    }
    return items.filter(
      (item) =>
        item.title.toLowerCase().includes(q) ||
        (item.subtitle && item.subtitle.toLowerCase().includes(q)) ||
        item.category.toLowerCase().includes(q),
    );
  }, [items, query, initialMode]);

  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelectedIndex((prev) => (filtered.length ? (prev + 1) % filtered.length : 0));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelectedIndex((prev) => (filtered.length ? (prev - 1 + filtered.length) % filtered.length : 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = filtered[selectedIndex];
      if (item) {
        item.action();
      }
    }
  };

  if (!open) return null;

  return (
    <div className="palette-backdrop" onClick={onClose} role="dialog" aria-modal="true" aria-label="Command Palette">
      <div className="palette-container" onClick={(e) => e.stopPropagation()}>
        <div className="palette-search-bar">
          <span className="palette-icon mono">{initialMode === "file" ? "🔍" : "⌘"}</span>
          <input
            ref={inputRef}
            type="text"
            className="palette-input mono"
            placeholder={
              initialMode === "file"
                ? "Type a filename to jump to (e.g. calc.py, ANCHOR.md)…"
                : "Type a command, slash task, or file name… (↑↓ to navigate, ↵ to run, ESC to close)"
            }
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
          />
          <kbd className="palette-kbd">ESC</kbd>
        </div>

        <div className="palette-results">
          {filtered.length === 0 ? (
            <div className="palette-empty small muted">No matching commands, files, or actions found.</div>
          ) : (
            filtered.map((item, idx) => {
              const isSelected = idx === selectedIndex;
              return (
                <div
                  key={item.id}
                  className={`palette-item ${isSelected ? "palette-item-selected" : ""}`}
                  onClick={() => item.action()}
                  onMouseEnter={() => setSelectedIndex(idx)}
                >
                  <div className="row" style={{ gap: "0.6rem" }}>
                    <span className="palette-item-icon">{item.icon}</span>
                    <div>
                      <div className="palette-item-title mono">
                        {item.title}
                        {item.badge && <span className="palette-badge">{item.badge}</span>}
                      </div>
                      {item.subtitle && <div className="palette-item-subtitle small muted">{item.subtitle}</div>}
                    </div>
                  </div>
                  {item.shortcut && <kbd className="palette-kbd">{item.shortcut}</kbd>}
                </div>
              );
            })
          )}
        </div>

        <div className="palette-footer">
          <span className="small muted mono">
            <kbd className="palette-kbd">↑↓</kbd> navigate · <kbd className="palette-kbd">↵</kbd> select ·{" "}
            <kbd className="palette-kbd">ESC</kbd> dismiss · <kbd className="palette-kbd">Ctrl+K</kbd> /{" "}
            <kbd className="palette-kbd">Ctrl+P</kbd>
          </span>
          <span className="small mono muted">Anchor Antigravity 2.0</span>
        </div>
      </div>
    </div>
  );
}
